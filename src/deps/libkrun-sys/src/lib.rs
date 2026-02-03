//! Low-level FFI bindings to libkrun
//!
//! This crate provides raw, unsafe bindings to the libkrun C library.
//! For a safe, idiomatic Rust API, use the higher-level wrapper in the runtime crate.

use std::os::raw::c_char;

// Log constants from libkrun.h
pub const KRUN_LOG_TARGET_DEFAULT: i32 = 0;
pub const KRUN_LOG_TARGET_STDOUT: i32 = 1;
pub const KRUN_LOG_TARGET_STDERR: i32 = 2;

pub const KRUN_LOG_LEVEL_OFF: u32 = 0;
pub const KRUN_LOG_LEVEL_ERROR: u32 = 1;
pub const KRUN_LOG_LEVEL_WARN: u32 = 2;
pub const KRUN_LOG_LEVEL_INFO: u32 = 3;
pub const KRUN_LOG_LEVEL_DEBUG: u32 = 4;
pub const KRUN_LOG_LEVEL_TRACE: u32 = 5;

pub const KRUN_LOG_STYLE_AUTO: u32 = 0;
pub const KRUN_LOG_STYLE_ALWAYS: u32 = 1;
pub const KRUN_LOG_STYLE_NEVER: u32 = 2;

// Disk format constants from libkrun.h
pub const KRUN_DISK_FORMAT_RAW: u32 = 0;
pub const KRUN_DISK_FORMAT_QCOW2: u32 = 1;

extern "C" {
    /// Initialize libkrun logging system.
    pub fn krun_init_log(target: i32, level: u32, style: u32, flags: u32) -> i32;

    /// Set the log level for libkrun.
    pub fn krun_set_log_level(level: u32) -> i32;

    /// Create a new libkrun context.
    /// Returns context ID on success, negative error code on failure.
    pub fn krun_create_ctx() -> i32;

    /// Free a libkrun context.
    pub fn krun_free_ctx(ctx_id: u32) -> i32;

    /// Configure VM resources (vCPUs and memory).
    pub fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;

    /// Set the root filesystem path for the VM.
    pub fn krun_set_root(ctx_id: u32, root_path: *const c_char) -> i32;

    /// Add a virtiofs mount to share a host directory with the guest.
    pub fn krun_add_virtiofs(
        ctx_id: u32,
        mount_tag: *const c_char,
        host_path: *const c_char,
    ) -> i32;

    /// Set a custom kernel for the VM.
    pub fn krun_set_kernel(
        ctx_id: u32,
        kernel_path: *const c_char,
        kernel_format: u32,
        initramfs: *const c_char,
        cmdline: *const c_char,
    ) -> i32;

    /// Set the executable to run inside the VM.
    pub fn krun_set_exec(
        ctx_id: u32,
        exec_path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> i32;

    /// Set environment variables for the VM.
    pub fn krun_set_env(ctx_id: u32, envp: *const *const c_char) -> i32;

    /// Set the working directory inside the VM.
    pub fn krun_set_workdir(ctx_id: u32, workdir_path: *const c_char) -> i32;

    /// Enable or disable split IRQ chip mode.
    pub fn krun_split_irqchip(ctx_id: u32, enable: bool) -> i32;

    /// Enable or disable nested virtualization.
    pub fn krun_set_nested_virt(ctx_id: u32, enabled: bool) -> i32;

    /// Set GPU options (virgl flags).
    pub fn krun_set_gpu_options(ctx_id: u32, virgl_flags: u32) -> i32;

    /// Set resource limits for the VM.
    pub fn krun_set_rlimits(ctx_id: u32, rlimits: *const *const c_char) -> i32;

    /// Set port mappings for the VM.
    pub fn krun_set_port_map(ctx_id: u32, port_map: *const *const c_char) -> i32;

    /// Add a vsock port bridged to a Unix socket.
    /// If listen is true, libkrun creates the socket and listens.
    /// If listen is false, libkrun connects to an existing socket.
    pub fn krun_add_vsock_port2(
        ctx_id: u32,
        port: u32,
        filepath: *const c_char,
        listen: bool,
    ) -> i32;

    /// Add a raw disk image to the VM.
    pub fn krun_add_disk(
        ctx_id: u32,
        block_id: *const c_char,
        disk_path: *const c_char,
        read_only: bool,
    ) -> i32;

    /// Add a disk image with explicit format specification.
    pub fn krun_add_disk2(
        ctx_id: u32,
        block_id: *const c_char,
        disk_path: *const c_char,
        disk_format: u32,
        read_only: bool,
    ) -> i32;

    /// Add a network backend via Unix stream socket.
    pub fn krun_add_net_unixstream(
        ctx_id: u32,
        c_path: *const c_char,
        fd: i32,
        c_mac: *const u8,
        features: u32,
        flags: u32,
    ) -> i32;

    /// Add a network backend via Unix datagram socket.
    pub fn krun_add_net_unixgram(
        ctx_id: u32,
        c_path: *const c_char,
        fd: i32,
        c_mac: *const u8,
        features: u32,
        flags: u32,
    ) -> i32;

    /// Start the VM and enter it (process takeover).
    /// On success, this function never returns.
    /// On failure, returns negative error code.
    /// On guest exit, returns the guest's exit status.
    pub fn krun_start_enter(ctx_id: u32) -> i32;

    /// Redirect VM console output to a file.
    pub fn krun_set_console_output(ctx_id: u32, filepath: *const c_char) -> i32;

    /// Set the uid before starting the microVM.
    pub fn krun_setuid(ctx_id: u32, uid: libc::uid_t) -> i32;

    /// Set the gid before starting the microVM.
    pub fn krun_setgid(ctx_id: u32, gid: libc::gid_t) -> i32;

    /// Configure a root filesystem backed by a block device with automatic remount.
    pub fn krun_set_root_disk_remount(
        ctx_id: u32,
        device: *const c_char,
        fstype: *const c_char,
        options: *const c_char,
    ) -> i32;
}
