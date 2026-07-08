//! `a3s-box info` command.

use clap::Args;
use std::path::Path;

use crate::state::BoxRecord;
use crate::state::StateFile;
use crate::status;

use super::images_dir;

const PNPM_CACHE_VOLUME_NAME: &str = "a3s-cache-pnpm";

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
        return;
    };

    match store.get(PNPM_CACHE_VOLUME_NAME) {
        Ok(Some(volume)) => {
            let size = directory_size(Path::new(&volume.mount_point)).unwrap_or(0);
            println!(
                "Package cache (pnpm): {} at {}",
                crate::output::format_bytes(size),
                volume.mount_point
            );
        }
        Ok(None) => println!("Package cache (pnpm): not created"),
        Err(error) => println!("Package cache (pnpm): unavailable ({error})"),
    }
}

fn print_host_mount_info() {
    let cache_mode = std::env::var("A3S_VIRTIOFS_CACHE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "none".to_string());
    println!("VirtioFS cache mode: {cache_mode}");
}

fn directory_size(path: &Path) -> std::io::Result<u64> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut total = 0_u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(directory_size(&entry.path())?);
    }
    Ok(total)
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
}
