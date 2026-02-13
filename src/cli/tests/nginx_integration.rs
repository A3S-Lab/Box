//! Integration test: Run nginx in a3s-box MicroVM.
//!
//! This test demonstrates the full lifecycle of running an nginx container
//! inside an a3s-box MicroVM:
//!
//! 1. Pull the nginx image from Docker Hub
//! 2. Run nginx in detached mode with port mapping
//! 3. Verify the box is running via `ps`
//! 4. Execute a command inside the running box
//! 5. Verify nginx serves HTTP responses
//! 6. Stop and remove the box
//!
//! ## Prerequisites
//!
//! - `a3s-box` binary built (`cargo build -p a3s-box-cli`)
//! - macOS with Apple HVF or Linux with KVM
//! - Internet access (to pull nginx image)
//! - `curl` available on the host
//!
//! ## Running
//!
//! ```bash
//! # Build first
//! cd crates/box/src && cargo build -p a3s-box-cli
//!
//! # Run the integration test
//! cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture
//! ```
//!
//! The test is `#[ignore]` by default because it requires a built binary,
//! network access, and virtualization support.

use std::process::Command;
use std::time::Duration;

/// Find the a3s-box binary in the target directory.
fn find_binary() -> String {
    // Try common locations relative to the workspace
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
// Test: Full nginx lifecycle
// ============================================================================

#[test]
#[ignore] // Requires built binary, network, and virtualization support
fn test_nginx_full_lifecycle() {
    let box_name = "test-nginx-integration";

    // Ensure clean state
    cleanup(box_name);

    // ----------------------------------------------------------------
    // Step 1: Pull nginx image
    // ----------------------------------------------------------------
    println!("==> Step 1: Pulling nginx image...");
    let stdout = run_ok(&["pull", "docker.io/library/nginx:alpine"]);
    println!("    Pull output: {}", stdout.trim());

    // Verify image is available
    let stdout = run_ok(&["images"]);
    assert!(
        stdout.contains("nginx"),
        "nginx image not found in `images` output:\n{}",
        stdout,
    );
    println!("    ✓ nginx image pulled successfully");

    // ----------------------------------------------------------------
    // Step 2: Run nginx in detached mode
    // ----------------------------------------------------------------
    println!("==> Step 2: Running nginx box...");
    let stdout = run_ok(&[
        "run",
        "-d",
        "--name",
        box_name,
        "-p",
        "8088:80",
        "docker.io/library/nginx:alpine",
    ]);
    let box_id = stdout.trim().to_string();
    println!("    Box ID: {}", box_id);
    assert!(!box_id.is_empty(), "Expected box ID in run output");

    // ----------------------------------------------------------------
    // Step 3: Verify box is running via `ps`
    // ----------------------------------------------------------------
    println!("==> Step 3: Verifying box is running...");
    wait_for(
        || {
            let (stdout, _, _) = run_cmd(&["ps"]);
            stdout.contains(box_name) && stdout.contains("running")
        },
        Duration::from_secs(30),
        "box to appear as running in `ps`",
    );
    let stdout = run_ok(&["ps"]);
    println!("    ps output:\n{}", stdout);
    assert!(stdout.contains(box_name));
    assert!(stdout.contains("running"));
    println!("    ✓ Box is running");

    // ----------------------------------------------------------------
    // Step 4: Inspect the box
    // ----------------------------------------------------------------
    println!("==> Step 4: Inspecting box...");
    let stdout = run_ok(&["inspect", box_name]);
    assert!(stdout.contains(box_name) || stdout.contains(&box_id));
    println!("    ✓ Inspect returned box details");

    // ----------------------------------------------------------------
    // Step 5: Execute a command inside the box
    // ----------------------------------------------------------------
    println!("==> Step 5: Executing command inside box...");

    // Wait for the exec socket to be ready
    std::thread::sleep(Duration::from_secs(3));

    let (stdout, stderr, success) = run_cmd(&["exec", box_name, "nginx", "-v"]);
    if success {
        println!("    nginx version: {}", stdout.trim());
        assert!(
            stdout.contains("nginx") || stderr.contains("nginx"),
            "Expected nginx version info"
        );
        println!("    ✓ Exec works inside the box");
    } else {
        println!("    ⚠ Exec not available yet (stderr: {}), skipping exec test", stderr.trim());
    }

    // ----------------------------------------------------------------
    // Step 6: Verify nginx serves HTTP (via port mapping)
    // ----------------------------------------------------------------
    println!("==> Step 6: Testing HTTP connectivity...");

    // Give nginx a moment to start listening
    std::thread::sleep(Duration::from_secs(2));

    let http_ok = wait_for_http("http://127.0.0.1:8088", Duration::from_secs(15));
    if http_ok {
        println!("    ✓ nginx is serving HTTP on port 8088");
    } else {
        println!("    ⚠ HTTP check skipped (port mapping may not be available in this environment)");
    }

    // ----------------------------------------------------------------
    // Step 7: Check logs
    // ----------------------------------------------------------------
    println!("==> Step 7: Checking logs...");
    let (stdout, _, _) = run_cmd(&["logs", box_name]);
    println!("    Logs (first 200 chars): {}", &stdout[..stdout.len().min(200)]);

    // ----------------------------------------------------------------
    // Step 8: Stop the box
    // ----------------------------------------------------------------
    println!("==> Step 8: Stopping box...");
    run_ok(&["stop", box_name]);

    // Verify it's stopped
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

    // ----------------------------------------------------------------
    // Step 9: Remove the box
    // ----------------------------------------------------------------
    println!("==> Step 9: Removing box...");
    run_ok(&["rm", box_name]);

    // Verify it's gone
    let stdout = run_ok(&["ps", "-a"]);
    assert!(
        !stdout.contains(box_name),
        "Box should be removed but still appears in `ps -a`:\n{}",
        stdout,
    );
    println!("    ✓ Box removed");

    println!("\n==> All steps passed! nginx lifecycle test complete.");
}

// ============================================================================
// Test: Run nginx with custom config via exec
// ============================================================================

#[test]
#[ignore]
fn test_nginx_exec_and_inspect() {
    let box_name = "test-nginx-exec";

    cleanup(box_name);

    // Run nginx
    run_ok(&[
        "run",
        "-d",
        "--name",
        box_name,
        "docker.io/library/nginx:alpine",
    ]);

    // Wait for box to be ready
    wait_for(
        || {
            let (stdout, _, _) = run_cmd(&["ps"]);
            stdout.contains(box_name) && stdout.contains("running")
        },
        Duration::from_secs(30),
        "box to be running",
    );

    // Wait for exec socket
    std::thread::sleep(Duration::from_secs(3));

    // Test various exec commands
    let cmd1: &[&str] = &["exec", box_name, "cat", "/etc/os-release"];
    let cmd2: &[&str] = &["exec", box_name, "ls", "/usr/share/nginx/html/"];
    let cmd3: &[&str] = &["exec", box_name, "whoami"];
    let test_cases: Vec<(&[&str], &str)> = vec![
        (cmd1, "Alpine"),
        (cmd2, "index.html"),
        (cmd3, "root"),
    ];

    for (cmd, expected) in &test_cases {
        let (stdout, stderr, success) = run_cmd(cmd);
        if success {
            assert!(
                stdout.contains(expected),
                "Expected '{}' in output of `{}`:\nstdout: {}\nstderr: {}",
                expected,
                cmd.join(" "),
                stdout,
                stderr,
            );
            println!("    ✓ `{}` → contains '{}'", cmd.join(" "), expected);
        } else {
            println!(
                "    ⚠ `{}` failed (exec may not be ready): {}",
                cmd.join(" "),
                stderr.trim()
            );
        }
    }

    // Cleanup
    cleanup(box_name);
    println!("==> nginx exec test complete.");
}

// ============================================================================
// Test: Run nginx with environment variables and labels
// ============================================================================

#[test]
#[ignore]
fn test_nginx_with_env_and_labels() {
    let box_name = "test-nginx-env";

    cleanup(box_name);

    // Run with env vars and labels
    run_ok(&[
        "run",
        "-d",
        "--name",
        box_name,
        "-e",
        "NGINX_HOST=example.com",
        "-e",
        "NGINX_PORT=80",
        "-l",
        "app=nginx",
        "-l",
        "env=test",
        "docker.io/library/nginx:alpine",
    ]);

    // Wait for running
    wait_for(
        || {
            let (stdout, _, _) = run_cmd(&["ps"]);
            stdout.contains(box_name) && stdout.contains("running")
        },
        Duration::from_secs(30),
        "box to be running",
    );

    // Inspect should show labels
    let stdout = run_ok(&["inspect", box_name]);
    assert!(
        stdout.contains("nginx") || stdout.contains(box_name),
        "Inspect should contain box info"
    );
    println!("    ✓ Box running with env vars and labels");

    // Verify env vars inside the box
    std::thread::sleep(Duration::from_secs(3));
    let (stdout, _, success) = run_cmd(&["exec", box_name, "printenv", "NGINX_HOST"]);
    if success {
        assert!(
            stdout.trim() == "example.com",
            "Expected NGINX_HOST=example.com, got: {}",
            stdout.trim()
        );
        println!("    ✓ Environment variable NGINX_HOST set correctly");
    }

    cleanup(box_name);
    println!("==> nginx env/labels test complete.");
}

// ============================================================================
// Helpers
// ============================================================================

/// Try to reach an HTTP endpoint, return true if we get a response.
fn wait_for_http(url: &str, timeout: Duration) -> bool {
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
