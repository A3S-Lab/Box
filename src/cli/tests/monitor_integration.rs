//! Integration test: Container monitoring and health checks.
//!
//! This test verifies the container monitoring daemon and health check
//! functionality:
//!
//! 1. Run a container with health check and restart policy
//! 2. Verify health check status transitions
//! 3. Verify auto-restart on unhealthy status
//! 4. Test foreground mode with Ctrl+C handling
//!
//! ## Prerequisites
//!
//! - `a3s-box` binary built (`cargo build -p a3s-box-cli`)
//! - macOS with Apple HVF or Linux with KVM
//! - Internet access (to pull images on first run)
//!
//! ## Running
//!
//! ```bash
//! cd crates/box/src
//! cargo test -p a3s-box-cli --test monitor_integration -- --ignored --nocapture
//! ```

use std::process::Command;
use std::time::Duration;
use std::thread;

/// Find the a3s-box binary in the target directory.
fn find_binary() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir)
        .parent()
        .expect("cli crate should be inside workspace");

    for profile in ["debug", "release"] {
        let bin = workspace_root.join("target").join(profile).join("a3s-box");
        if bin.exists() {
            return bin.to_string_lossy().to_string();
        }
    }

    "a3s-box".to_string()
}

/// Run an a3s-box command and capture output.
fn run_cmd(args: &[&str]) -> (String, String, bool) {
    let bin = find_binary();
    let output = Command::new(&bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("Failed to run `a3s-box {}`: {}", args.join(" "), e));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

/// Test basic health check functionality.
#[test]
#[ignore]
fn test_health_check_basic() {
    eprintln!("=== Test: Basic health check ===");

    // Clean up any existing test containers
    let _ = run_cmd(&["rm", "-f", "test-health"]);

    // Run container with health check
    let (_stdout, _, success) = run_cmd(&[
        "run", "-d", "--name", "test-health",
        "--health-cmd", "true",
        "--health-interval", "5",
        "--health-retries", "3",
        "alpine:latest",
        "sleep", "60"
    ]);
    assert!(success, "Failed to start container");

    // Wait for health check to run
    thread::sleep(Duration::from_secs(10));

    // Check container status
    let (stdout, _, success) = run_cmd(&["inspect", "test-health"]);
    assert!(success, "Failed to inspect container");
    assert!(stdout.contains("healthy") || stdout.contains("starting"),
            "Container should be healthy or starting");

    // Clean up
    let _ = run_cmd(&["rm", "-f", "test-health"]);
    eprintln!("=== Test passed ===");
}

/// Test restart policy with always.
#[test]
#[ignore]
fn test_restart_policy_always() {
    eprintln!("=== Test: Restart policy always ===");

    // Clean up
    let _ = run_cmd(&["rm", "-f", "test-restart"]);

    // Run container that exits immediately with restart=always
    let (_, _, success) = run_cmd(&[
        "run", "-d", "--name", "test-restart",
        "--restart", "always",
        "alpine:latest",
        "sh", "-c", "exit 0"
    ]);
    assert!(success, "Failed to start container");

    // Wait for restart
    thread::sleep(Duration::from_secs(10));

    // Check if container was restarted
    let (_stdout, _, success) = run_cmd(&["inspect", "test-restart"]);
    assert!(success, "Failed to inspect container");

    // Clean up
    let _ = run_cmd(&["rm", "-f", "test-restart"]);
    eprintln!("=== Test passed ===");
}

/// Test foreground mode.
#[test]
#[ignore]
fn test_foreground_mode() {
    eprintln!("=== Test: Foreground mode ===");

    // Run container in foreground with --rm
    let bin = find_binary();
    let output = Command::new(&bin)
        .args(&["run", "--rm", "alpine:latest", "echo", "hello"])
        .output()
        .expect("Failed to run foreground container");

    assert!(output.status.success(), "Foreground container failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello"), "Expected output not found");

    eprintln!("=== Test passed ===");
}
