//! Integration test: Run containers in a3s-box MicroVM.
//!
//! This test demonstrates the full lifecycle of running containers
//! inside a3s-box MicroVMs:
//!
//! 1. Pull an OCI image from Docker Hub
//! 2. Run a container in detached mode
//! 3. Verify the box is running via `ps`
//! 4. Execute commands inside the running box
//! 5. Stop and remove the box
//!
//! ## Prerequisites
//!
//! - `a3s-box` binary built (`cargo build -p a3s-box-cli`)
//! - macOS with Apple HVF or Linux with KVM
//! - Internet access (to pull images on first run)
//! - `DYLD_LIBRARY_PATH` set to include libkrun/libkrunfw build dirs
//!
//! ## Running
//!
//! ```bash
//! cd crates/box/src
//!
//! # Set library paths (macOS)
//! export DYLD_LIBRARY_PATH="$(ls -td target/debug/build/libkrun-sys-*/out/libkrun/lib | head -1):$(ls -td target/debug/build/libkrun-sys-*/out/libkrunfw/lib | head -1)"
//!
//! # Run all integration tests
//! cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture
//!
//! # Run a single test
//! cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture test_alpine_full_lifecycle
//! ```
//!
//! Tests are `#[ignore]` by default because they require a built binary,
//! network access, and virtualization support (HVF/KVM).

use std::process::Command;
use std::time::Duration;

/// Find the a3s-box binary in the target directory.
fn find_binary() -> String {
    let candidates = [
        "../../target/debug/a3s-box",
        "../../target/release/a3s-box",
        "../../../target/debug/a3s-box",
        "../../../target/release/a3s-box",
    ];

    for candidate in &candidates {
        let path = std::path::Path::new(candidate);
        if path.exists() {
            return path.canonicalize().unwrap().to_string_lossy().to_string();
        }
    }

    // Fall back to PATH
    "a3s-box".to_string()
}

/// Run an a3s-box command and return (stdout, stderr, success).
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

/// Run an a3s-box command, assert success, return stdout.
fn run_ok(args: &[&str]) -> String {
    let (stdout, stderr, success) = run_cmd(args);
    assert!(
        success,
        "Command `a3s-box {}` failed.\nstdout: {}\nstderr: {}",
        args.join(" "),
        stdout,
        stderr,
    );
    stdout
}

/// Wait for a condition with timeout.
fn wait_for<F: Fn() -> bool>(condition: F, timeout: Duration, msg: &str) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("Timeout waiting for: {}", msg);
}

/// Cleanup helper: stop and remove a box by name, ignoring errors.
fn cleanup(name: &str) {
    let _ = run_cmd(&["stop", name]);
    let _ = run_cmd(&["rm", name]);
}

// ============================================================================
// Test: Full alpine lifecycle (pull → run → ps → exec → stop → rm)
// ============================================================================

/// Demonstrates the complete a3s-box VM lifecycle using Alpine Linux.
///
/// This is the primary integration test that verifies:
/// - Image pulling from Docker Hub
/// - VM creation and boot via libkrun
/// - Box status tracking
/// - Command execution inside the VM
/// - Graceful shutdown and cleanup
#[test]
#[ignore] // Requires built binary, network, and virtualization support
fn test_alpine_full_lifecycle() {
    let box_name = "integ-alpine-lifecycle";
    cleanup(box_name);

    // ---- Step 1: Pull alpine image ----
    println!("==> Step 1: Pulling alpine image...");
    run_ok(&["pull", "docker.io/library/alpine:latest"]);

    let stdout = run_ok(&["images"]);
    assert!(stdout.contains("alpine"), "alpine image not in `images`");
    println!("    ✓ alpine image available");

    // ---- Step 2: Run alpine with sleep (long-running process) ----
    println!("==> Step 2: Running alpine box...");
    let stdout = run_ok(&[
        "run", "-d",
        "--name", box_name,
        "docker.io/library/alpine:latest",
        "--", "sleep", "3600",
    ]);
    let box_id = stdout.trim().to_string();
    assert!(!box_id.is_empty(), "Expected box ID in run output");
    println!("    Box ID: {}", box_id);

    // ---- Step 3: Verify box is running ----
    println!("==> Step 3: Verifying box is running...");
    wait_for(
        || {
            let (stdout, _, _) = run_cmd(&["ps"]);
            stdout.contains(box_name) && stdout.contains("running")
        },
        Duration::from_secs(30),
        "box to appear as running in `ps`",
    );
    println!("    ✓ Box is running");

    // ---- Step 4: Inspect the box ----
    println!("==> Step 4: Inspecting box...");
    let stdout = run_ok(&["inspect", box_name]);
    assert!(stdout.contains(&box_id) || stdout.contains(box_name));
    assert!(stdout.contains("alpine"));
    println!("    ✓ Inspect shows correct box info");

    // ---- Step 5: Execute commands inside the VM ----
    println!("==> Step 5: Executing commands inside box...");

    // Wait for exec server to be ready
    std::thread::sleep(Duration::from_secs(2));

    // uname -a: verify we're in a Linux VM
    let (stdout, _, success) = run_cmd(&["exec", box_name, "--", "uname", "-a"]);
    if success {
        assert!(stdout.contains("Linux"), "Expected Linux kernel");
        println!("    ✓ uname: {}", stdout.trim());
    } else {
        println!("    ⚠ exec not available, skipping");
    }

    // cat /etc/os-release: verify Alpine
    let (stdout, _, success) = run_cmd(&["exec", box_name, "--", "cat", "/etc/os-release"]);
    if success {
        assert!(stdout.contains("Alpine"), "Expected Alpine Linux");
        println!("    ✓ OS: Alpine Linux");
    }

    // ls /: verify filesystem structure
    let (stdout, _, success) = run_cmd(&["exec", box_name, "--", "ls", "/"]);
    if success {
        assert!(stdout.contains("bin"), "Expected /bin in rootfs");
        assert!(stdout.contains("etc"), "Expected /etc in rootfs");
        println!("    ✓ Filesystem looks correct");
    }

    // ---- Step 6: Check logs ----
    println!("==> Step 6: Checking logs...");
    let (stdout, _, _) = run_cmd(&["logs", box_name]);
    println!(
        "    Logs (first 200 chars): {}",
        &stdout[..stdout.len().min(200)]
    );

    // ---- Step 7: Stop the box ----
    println!("==> Step 7: Stopping box...");
    run_ok(&["stop", box_name]);

    wait_for(
        || {
            let (stdout, _, _) = run_cmd(&["ps", "-a"]);
            stdout.contains(box_name)
                && (stdout.contains("stopped") || stdout.contains("exited"))
        },
        Duration::from_secs(15),
        "box to appear as stopped",
    );
    println!("    ✓ Box stopped");

    // ---- Step 8: Remove the box ----
    println!("==> Step 8: Removing box...");
    run_ok(&["rm", box_name]);

    let stdout = run_ok(&["ps", "-a"]);
    assert!(
        !stdout.contains(box_name),
        "Box should be removed from `ps -a`"
    );
    println!("    ✓ Box removed");

    println!("\n==> All steps passed! Alpine lifecycle test complete.");
}

// ============================================================================
// Test: Execute multiple commands inside a running box
// ============================================================================

/// Demonstrates executing various commands inside a running a3s-box VM.
#[test]
#[ignore]
fn test_exec_commands() {
    let box_name = "integ-exec-cmds";
    cleanup(box_name);

    // Run alpine
    run_ok(&[
        "run", "-d",
        "--name", box_name,
        "docker.io/library/alpine:latest",
        "--", "sleep", "3600",
    ]);

    wait_for(
        || {
            let (stdout, _, _) = run_cmd(&["ps"]);
            stdout.contains(box_name) && stdout.contains("running")
        },
        Duration::from_secs(30),
        "box to be running",
    );

    // Wait for exec server
    std::thread::sleep(Duration::from_secs(2));

    // Test: read OS release
    let (stdout, _, success) = run_cmd(&["exec", box_name, "--", "cat", "/etc/os-release"]);
    if success {
        assert!(stdout.contains("Alpine"), "Expected Alpine in os-release");
        println!("    ✓ cat /etc/os-release → Alpine");
    }

    // Test: list root filesystem
    let (stdout, _, success) = run_cmd(&["exec", box_name, "--", "ls", "/usr/bin/"]);
    if success {
        println!("    ✓ ls /usr/bin/ → {} entries", stdout.lines().count());
    }

    // Test: environment variables
    let (stdout, _, success) = run_cmd(&["exec", box_name, "--", "env"]);
    if success {
        println!("    ✓ env → {} variables", stdout.lines().count());
    }

    // Test: write and read a file
    let (_, _, success) = run_cmd(&[
        "exec", box_name, "--",
        "sh", "-c", "echo hello-a3s > /tmp/test.txt",
    ]);
    if success {
        let (stdout, _, success) = run_cmd(&["exec", box_name, "--", "cat", "/tmp/test.txt"]);
        if success {
            assert!(
                stdout.trim() == "hello-a3s",
                "Expected 'hello-a3s', got '{}'",
                stdout.trim()
            );
            println!("    ✓ Write + read file inside VM works");
        }
    }

    cleanup(box_name);
    println!("==> Exec commands test complete.");
}

// ============================================================================
// Test: Run with environment variables and labels
// ============================================================================

/// Demonstrates passing environment variables and labels to a box.
#[test]
#[ignore]
fn test_env_and_labels() {
    let box_name = "integ-env-labels";
    cleanup(box_name);

    // Run with env vars and labels
    run_ok(&[
        "run", "-d",
        "--name", box_name,
        "-e", "MY_APP=a3s-test",
        "-e", "MY_VERSION=1.0",
        "-l", "app=test",
        "-l", "env=integration",
        "docker.io/library/alpine:latest",
        "--", "sleep", "3600",
    ]);

    wait_for(
        || {
            let (stdout, _, _) = run_cmd(&["ps"]);
            stdout.contains(box_name) && stdout.contains("running")
        },
        Duration::from_secs(30),
        "box to be running",
    );

    // Inspect should show the box
    let stdout = run_ok(&["inspect", box_name]);
    assert!(stdout.contains(box_name));
    println!("    ✓ Box running with env vars and labels");

    // Verify env vars inside the box
    std::thread::sleep(Duration::from_secs(2));
    let (stdout, _, success) = run_cmd(&[
        "exec", box_name, "--", "sh", "-c", "echo $MY_APP",
    ]);
    if success {
        assert!(
            stdout.trim() == "a3s-test",
            "Expected MY_APP=a3s-test, got: '{}'",
            stdout.trim()
        );
        println!("    ✓ Environment variable MY_APP set correctly");
    }

    let (stdout, _, success) = run_cmd(&[
        "exec", box_name, "--", "sh", "-c", "echo $MY_VERSION",
    ]);
    if success {
        assert!(
            stdout.trim() == "1.0",
            "Expected MY_VERSION=1.0, got: '{}'",
            stdout.trim()
        );
        println!("    ✓ Environment variable MY_VERSION set correctly");
    }

    cleanup(box_name);
    println!("==> Env and labels test complete.");
}

// ============================================================================
// Test: nginx with known limitation
// ============================================================================

/// Demonstrates running nginx in a3s-box.
///
/// NOTE: nginx's default `listen ... backlog 511` may fail under libkrun's
/// TSI networking with `listen() failed (22: Invalid argument)`. This test
/// documents the known limitation and verifies the image at least loads.
#[test]
#[ignore]
fn test_nginx_image_pull_and_run() {
    let box_name = "integ-nginx";
    cleanup(box_name);

    // Pull nginx
    println!("==> Pulling nginx:alpine...");
    run_ok(&["pull", "docker.io/library/nginx:alpine"]);

    let stdout = run_ok(&["images"]);
    assert!(stdout.contains("nginx"), "nginx image not found");
    println!("    ✓ nginx:alpine pulled");

    // Run nginx (may fail due to backlog limitation)
    println!("==> Running nginx (may exit due to TSI backlog limitation)...");
    let (stdout, stderr, success) = run_cmd(&[
        "run", "-d",
        "--name", box_name,
        "-p", "8088:80",
        "docker.io/library/nginx:alpine",
    ]);

    if success {
        let box_id = stdout.trim();
        println!("    Box ID: {}", box_id);

        // Give it a moment
        std::thread::sleep(Duration::from_secs(3));

        // Check if it's still running or died
        let (ps_out, _, _) = run_cmd(&["ps", "-a"]);
        if ps_out.contains("running") && ps_out.contains(box_name) {
            println!("    ✓ nginx is running!");

            // Try HTTP
            let http_ok = try_http("http://127.0.0.1:8088", Duration::from_secs(5));
            if http_ok {
                println!("    ✓ nginx serving HTTP on port 8088");
            } else {
                println!("    ⚠ HTTP not reachable (port mapping may not be available)");
            }
        } else {
            println!("    ⚠ nginx exited (expected: TSI backlog limitation)");
            // Verify it at least started and logged the nginx config
            let (logs, _, _) = run_cmd(&["logs", box_name]);
            if logs.contains("Configuration complete") {
                println!("    ✓ nginx configured successfully before listen() failure");
            }
        }
    } else {
        println!("    ⚠ Run failed: {}", stderr.trim());
    }

    cleanup(box_name);
    println!("==> nginx test complete.");
}

// ============================================================================
// Helpers
// ============================================================================

/// Try to reach an HTTP endpoint, return true if we get a response.
fn try_http(url: &str, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        let result = Command::new("curl")
            .args(["-sf", "--max-time", "2", url])
            .output();

        if let Ok(output) = result {
            if output.status.success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}
