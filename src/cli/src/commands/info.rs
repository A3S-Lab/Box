//! `a3s-box info` command.

use clap::Args;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::state::BoxRecord;
use crate::state::StateFile;
use crate::status;

use super::images_dir;

const PNPM_CACHE_VOLUME_NAME: &str = "a3s-cache-pnpm";
const NPM_CACHE_VOLUME_NAME: &str = "a3s-cache-npm";
const PACKAGE_CACHE_SIZE_ENV: &str = "A3S_BOX_INFO_CACHE_SIZE";
const PACKAGE_CACHE_SIZE_BUDGET: Duration = Duration::from_millis(500);
const DEFAULT_POOL_SOCKET: &str = "/tmp/a3s-box-pool.sock";
const RUN_POOL_SOCKET_ENV: &str = "A3S_BOX_RUN_POOL_SOCKET";
const BUILD_RUN_POOL_SOCKET_ENV: &str = "A3S_BOX_BUILD_RUN_POOL_SOCKET";

#[derive(Args)]
pub struct InfoArgs;

pub async fn execute(_args: InfoArgs) -> Result<(), Box<dyn std::error::Error>> {
    println!("a3s-box version {}", a3s_box_core::VERSION);
    let capabilities = a3s_box_core::PlatformCapabilities::current();

    // Virtualization support
    match a3s_box_runtime::check_virtualization_support() {
        Ok(support) => {
            println!("Virtualization: {} ({})", support.backend, support.details);
        }
        Err(e) => {
            println!("Virtualization: not available ({e})");
        }
    }

    // Home directory
    let home = a3s_box_core::dirs_home();
    println!("Home directory: {}", home.display());
    print_capabilities(&capabilities);

    // Box count
    match StateFile::load_default() {
        Ok(state) => {
            let counts = box_counts(&state);
            println!(
                "Boxes: {} total, {} active ({} running, {} paused)",
                counts.total, counts.active, counts.running, counts.paused
            );
        }
        Err(_) => {
            println!("Boxes: 0 total, 0 active (0 running, 0 paused)");
        }
    }

    // Image cache stats
    let images_dir = images_dir();
    if images_dir.exists() {
        match super::open_image_store() {
            Ok(store) => {
                let images = store.list().await;
                let total_size: u64 = images.iter().map(|i| i.size_bytes).sum();
                println!(
                    "Images: {} cached ({})",
                    images.len(),
                    crate::output::format_bytes(total_size)
                );
            }
            Err(_) => {
                println!("Images: 0 cached");
            }
        }
    } else {
        println!("Images: 0 cached");
    }
    print_package_cache_info();
    print_host_mount_info();
    print_rootfs_symlink_info(&home);
    print_warm_pool_info().await;

    Ok(())
}

fn print_capabilities(capabilities: &a3s_box_core::PlatformCapabilities) {
    println!(
        "Host platform: {}/{}",
        capabilities.os, capabilities.architecture
    );
    println!("VM backend: {}", capabilities.vm_backend);
    println!("Control channel: {}", capabilities.host_guest_channel);
    println!(
        "Bridge networking: {}",
        capabilities.bridge_networking_summary()
    );
    println!(
        "Published ports: {}",
        if capabilities.published_ports {
            "tcp"
        } else {
            "unsupported"
        }
    );
    println!(
        "TEE: attestation {}, sealed storage {}",
        availability(capabilities.tee_attestation),
        availability(capabilities.sealed_storage)
    );
}

fn availability(value: bool) -> &'static str {
    if value {
        "available"
    } else {
        "unavailable"
    }
}

fn print_package_cache_info() {
    let Ok(store) = a3s_box_runtime::VolumeStore::default_path() else {
        println!("Package cache (pnpm): unavailable");
        println!("Package cache (npm): unavailable");
        return;
    };

    print_named_package_cache(&store, "pnpm", PNPM_CACHE_VOLUME_NAME);
    print_named_package_cache(&store, "npm", NPM_CACHE_VOLUME_NAME);
}

fn print_named_package_cache(store: &a3s_box_runtime::VolumeStore, label: &str, volume_name: &str) {
    match store.get(volume_name) {
        Ok(Some(volume)) => {
            if !scan_package_cache_size_enabled() {
                println!(
                    "Package cache ({label}): created at {} (size scan skipped; set {PACKAGE_CACHE_SIZE_ENV}=1 to enable)",
                    volume.mount_point
                );
                return;
            }

            match directory_size_bounded(Path::new(&volume.mount_point), PACKAGE_CACHE_SIZE_BUDGET)
            {
                Ok(DirectorySize::Complete(size)) => {
                    println!(
                        "Package cache ({label}): {} at {}",
                        crate::output::format_bytes(size),
                        volume.mount_point
                    );
                }
                Ok(DirectorySize::TimedOut(partial)) => {
                    println!(
                        "Package cache ({label}): at least {} at {} (size scan timed out after {}ms)",
                        crate::output::format_bytes(partial),
                        volume.mount_point,
                        PACKAGE_CACHE_SIZE_BUDGET.as_millis()
                    );
                }
                Err(error) => println!("Package cache ({label}): unavailable ({error})"),
            }
        }
        Ok(None) => println!("Package cache ({label}): not created"),
        Err(error) => println!("Package cache ({label}): unavailable ({error})"),
    }
}

fn scan_package_cache_size_enabled() -> bool {
    std::env::var(PACKAGE_CACHE_SIZE_ENV)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn print_host_mount_info() {
    let cache_mode = std::env::var("A3S_VIRTIOFS_CACHE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "none".to_string());
    println!("VirtioFS cache mode: {cache_mode}");
}

#[cfg(windows)]
fn print_rootfs_symlink_info(home: &Path) {
    match probe_windows_symlink_support(home) {
        Ok(()) => println!("OCI symlink support: available"),
        Err(error) if error.raw_os_error() == Some(1314) => println!(
            "OCI symlink support: unavailable (enable Windows Developer Mode or grant \
             SeCreateSymbolicLinkPrivilege; ERROR_PRIVILEGE_NOT_HELD (1314))"
        ),
        Err(error) => println!("OCI symlink support: unavailable (probe failed: {error})"),
    }
}

#[cfg(windows)]
fn probe_windows_symlink_support(home: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    let directory = tempfile::Builder::new()
        .prefix(".symlink-capability-")
        .tempdir_in(home)?;
    std::fs::write(directory.path().join("target"), b"probe")?;
    std::os::windows::fs::symlink_file("target", directory.path().join("link"))
}

#[cfg(not(windows))]
fn print_rootfs_symlink_info(_home: &Path) {
    println!("OCI symlink support: native");
}

#[cfg(not(windows))]
async fn print_warm_pool_info() {
    let sockets = warm_pool_info_sockets();
    for socket in &sockets {
        match a3s_box_runtime::pool::client::status_client(socket).await {
            Ok(status) => {
                print_warm_pool_status(socket, &status.images);
                return;
            }
            Err(_) => continue,
        }
    }

    println!(
        "Warm pool daemon: not running (checked {})",
        sockets.join(", ")
    );
}

#[cfg(windows)]
async fn print_warm_pool_info() {
    println!("Warm pool daemon: unsupported on Windows");
}

#[cfg(not(windows))]
fn warm_pool_info_sockets() -> Vec<String> {
    warm_pool_info_sockets_from(
        std::env::var(RUN_POOL_SOCKET_ENV).ok().as_deref(),
        std::env::var(BUILD_RUN_POOL_SOCKET_ENV).ok().as_deref(),
        DEFAULT_POOL_SOCKET,
    )
}

#[cfg(not(windows))]
fn warm_pool_info_sockets_from(
    run_socket: Option<&str>,
    build_socket: Option<&str>,
    default_socket: &str,
) -> Vec<String> {
    let mut sockets = Vec::new();
    for socket in [run_socket, build_socket, Some(default_socket)]
        .into_iter()
        .flatten()
        .map(str::trim)
    {
        if !socket.is_empty() && !sockets.iter().any(|existing| existing == socket) {
            sockets.push(socket.to_string());
        }
    }
    sockets
}

#[cfg(not(windows))]
fn print_warm_pool_status(socket: &str, images: &[a3s_box_runtime::pool::PoolImageStat]) {
    if images.is_empty() {
        println!("Warm pool daemon: running at {socket} (no warm pools yet)");
        return;
    }

    let max: usize = images.iter().map(|image| image.max).sum();
    let idle: usize = images.iter().map(|image| image.idle).sum();
    let active: usize = images.iter().map(|image| image.active).sum();
    let leased: usize = images.iter().map(|image| image.leased).sum();
    println!(
        "Warm pool daemon: running at {socket} ({} pools, max {}, {} idle, {} active, {} leased)",
        images.len(),
        max,
        idle,
        active,
        leased
    );
}

#[cfg(test)]
fn directory_size(path: &Path) -> std::io::Result<u64> {
    match directory_size_inner(path, None)? {
        DirectorySize::Complete(size) | DirectorySize::TimedOut(size) => Ok(size),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectorySize {
    Complete(u64),
    TimedOut(u64),
}

fn directory_size_bounded(path: &Path, budget: Duration) -> std::io::Result<DirectorySize> {
    let deadline = Instant::now()
        .checked_add(budget)
        .unwrap_or_else(Instant::now);
    directory_size_inner(path, Some(deadline))
}

fn directory_size_inner(path: &Path, deadline: Option<Instant>) -> std::io::Result<DirectorySize> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(DirectorySize::TimedOut(0));
    }

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DirectorySize::Complete(0));
        }
        Err(error) => return Err(error),
    };
    if metadata.is_file() {
        return Ok(DirectorySize::Complete(metadata.len()));
    }
    if !metadata.is_dir() {
        return Ok(DirectorySize::Complete(0));
    }

    let mut total = 0_u64;
    for entry in std::fs::read_dir(path)? {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(DirectorySize::TimedOut(total));
        }

        let entry = entry?;
        match directory_size_inner(&entry.path(), deadline)? {
            DirectorySize::Complete(size) => {
                total = total.saturating_add(size);
            }
            DirectorySize::TimedOut(size) => {
                return Ok(DirectorySize::TimedOut(total.saturating_add(size)));
            }
        }
    }
    Ok(DirectorySize::Complete(total))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoxCounts {
    total: usize,
    active: usize,
    running: usize,
    paused: usize,
}

fn box_counts(state: &StateFile) -> BoxCounts {
    box_counts_from_records(state.list(true))
}

fn box_counts_from_records(records: Vec<&BoxRecord>) -> BoxCounts {
    let total = records.len();
    let running = records
        .iter()
        .filter(|record| record.status == "running")
        .count();
    let paused = records
        .iter()
        .filter(|record| record.status == "paused")
        .count();
    let active = records
        .iter()
        .filter(|record| status::is_active(record))
        .count();

    BoxCounts {
        total,
        active,
        running,
        paused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_box_counts_include_paused_as_active() {
        let (_tmp, state) = setup_state(vec![
            make_record("id-1", "running", "running", Some(1)),
            make_record("id-2", "paused", "paused", Some(1)),
            make_record("id-3", "created", "created", None),
            make_record("id-4", "stopped", "stopped", None),
            make_record("id-5", "dead", "dead", None),
        ]);

        assert_eq!(
            box_counts(&state),
            BoxCounts {
                total: 5,
                active: 2,
                running: 1,
                paused: 1,
            }
        );
    }

    #[test]
    fn test_availability_labels() {
        assert_eq!(availability(true), "available");
        assert_eq!(availability(false), "unavailable");
    }

    #[test]
    fn test_directory_size_sums_regular_files_without_following_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("one"), b"1234").unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        std::fs::write(tmp.path().join("nested").join("two"), b"12").unwrap();

        assert_eq!(directory_size(tmp.path()).unwrap(), 6);
        assert_eq!(directory_size(&tmp.path().join("missing")).unwrap(), 0);
    }

    #[test]
    fn test_directory_size_bounded_reports_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("one"), b"1234").unwrap();

        assert_eq!(
            directory_size_bounded(tmp.path(), Duration::ZERO).unwrap(),
            DirectorySize::TimedOut(0)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_warm_pool_info_sockets_prefers_env_and_deduplicates() {
        assert_eq!(
            warm_pool_info_sockets_from(
                Some(" /tmp/runtime.sock "),
                Some("/tmp/build.sock"),
                DEFAULT_POOL_SOCKET,
            ),
            vec![
                "/tmp/runtime.sock".to_string(),
                "/tmp/build.sock".to_string(),
                DEFAULT_POOL_SOCKET.to_string(),
            ]
        );
        assert_eq!(
            warm_pool_info_sockets_from(
                Some(" /tmp/runtime.sock "),
                Some("/tmp/runtime.sock"),
                "/tmp/runtime.sock",
            ),
            vec!["/tmp/runtime.sock".to_string()]
        );
    }
}
