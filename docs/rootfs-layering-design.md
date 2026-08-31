# Rootfs Layering Design — Overlay-based CoW Boot

> This document describes the directory-layering milestone. The current VMM
> contract uses a typed `RootfsSource`, and the macOS target is a guest-native
> raw ext4 block artifact. See [Guest-Native Rootfs Design](guest-native-rootfs-design.md).

## Problem

Every box boot does a full `copy_dir_recursive` of the rootfs (~500MB for Alpine+Python), even on cache hit. This is the #1 cold-start bottleneck.

```
Current: Cache Hit → copy_dir_recursive (500MB) → box_dir/rootfs/ → VM boot
Target:  Cache Hit → overlayfs mount (<1ms)     → box_dir/merged/ → VM boot
```

## Architecture (First Principles)

### Core Change (1 component)

**`OverlayMount`** — manages host-side overlayfs mounts for box rootfs.

```
~/.a3s/cache/rootfs/<key>/   ← lower (read-only, shared across boxes)
~/.a3s/boxes/<id>/upper/     ← upper (per-box writes, CoW)
~/.a3s/boxes/<id>/work/      ← overlayfs workdir
~/.a3s/boxes/<id>/merged/    ← merged view → RootfsSource::Directory
```

### Extension Point (1 trait)

**`RootfsProvider`** trait — abstracts how a rootfs directory is prepared.

```rust
pub trait RootfsProvider: Send + Sync {
    /// Prepare the host-side staging directory for a box.
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf>;

    /// Choose the directory or block transport after host-side preparation.
    fn finalize_for_boot(
        &self,
        box_dir: &Path,
        staged_rootfs: &Path,
        options: RootfsFinalizeOptions,
    ) -> Result<RootfsSource>;

    /// Cleanup after box stops (unmount overlay, remove upper/work).
    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()>;
}
```

Default implementations:
- `CopyProvider` — current behavior (full copy), zero-dependency fallback
- `OverlayProvider` — overlayfs mount (Linux only, requires root or unprivileged overlayfs)

### What Doesn't Change

- `CacheBackend` trait — unchanged
- `RootfsCache` — still caches the fully-built rootfs as the read-only lower layer
- `RootfsSource::Directory` points to `merged/` instead of `rootfs/` on Linux
- guest-init remains unaware of the host directory-layering implementation
- CLI commands — transparent

## Detailed Design

### 1. New Files

```
runtime/src/rootfs/
  ├── mod.rs          ← existing (add re-exports)
  ├── layout.rs       ← existing (unchanged)
  ├── builder.rs      ← existing (unchanged)
  ├── provider.rs     ← NEW: RootfsProvider trait + CopyProvider + OverlayProvider
  └── overlay.rs      ← NEW: OverlayMount (mount/unmount/cleanup)
```

### 2. `RootfsProvider` Trait

```rust
// runtime/src/rootfs/provider.rs

use std::path::{Path, PathBuf};
use a3s_box_core::error::Result;

/// Abstracts how a rootfs directory is prepared for a box.
pub trait RootfsProvider: Send + Sync {
    /// Prepare a rootfs at box_dir from the cached lower layer at cache_dir.
    /// Returns the host-side staging directory.
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf>;

    fn finalize_for_boot(
        &self,
        box_dir: &Path,
        staged_rootfs: &Path,
        options: RootfsFinalizeOptions,
    ) -> Result<RootfsSource>;

    /// Cleanup after box stops.
    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()>;

    /// Whether this provider supports the current platform.
    fn is_available(&self) -> bool;
}
```

### 3. `CopyProvider` (Fallback)

Current behavior, extracted into the trait:

```rust
pub struct CopyProvider;

impl RootfsProvider for CopyProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let rootfs = box_dir.join("rootfs");
        copy_dir_recursive(cache_dir, &rootfs)?;
        Ok(rootfs)
    }

    fn cleanup(&self, box_dir: &Path) -> Result<()> {
        let rootfs = box_dir.join("rootfs");
        if rootfs.exists() {
            std::fs::remove_dir_all(&rootfs).ok();
        }
        Ok(())
    }

    fn is_available(&self) -> bool { true }
}
```

### 4. `OverlayProvider` (Linux)

```rust
pub struct OverlayProvider;

impl RootfsProvider for OverlayProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let upper = box_dir.join("upper");
        let work = box_dir.join("work");
        let merged = box_dir.join("merged");

        std::fs::create_dir_all(&upper)?;
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(&merged)?;

        overlay_mount(cache_dir, &upper, &work, &merged)?;
        Ok(merged)
    }

    fn cleanup(&self, box_dir: &Path) -> Result<()> {
        let merged = box_dir.join("merged");
        if merged.exists() {
            overlay_unmount(&merged).ok();
        }
        // Remove upper/work/merged dirs
        for dir in &["upper", "work", "merged"] {
            let p = box_dir.join(dir);
            if p.exists() { std::fs::remove_dir_all(&p).ok(); }
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        cfg!(target_os = "linux") && check_overlay_support()
    }
}
```

### 5. `overlay.rs` — Mount Operations

```rust
// Linux overlayfs mount via libc::mount or Command::new("mount")

fn overlay_mount(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(), upper.display(), work.display()
    );

    // Try unprivileged overlayfs first (Linux 5.11+, in user namespace)
    // Fall back to mount(2) syscall
    // Fall back to `mount -t overlay` command
}

fn overlay_unmount(merged: &Path) -> Result<()> {
    // umount(2) or `umount` command
}

fn check_overlay_support() -> bool {
    // Check /proc/filesystems for "overlay"
    // Try a test mount in a tempdir
}
```

### 6. Integration in `prepare_layout()`

Current flow in `vm/mod.rs`:
```rust
// Cache hit: copy_dir_recursive(cache → rootfs)
// Cache miss: build rootfs → copy_dir_recursive(rootfs → cache)
```

New flow:
```rust
// Cache hit: rootfs_provider.prepare(box_dir, cache_dir)
//   → OverlayProvider: mount overlay (<1ms)
//   → CopyProvider: copy_dir_recursive (fallback)
// Cache miss: build rootfs → copy into cache → rootfs_provider.prepare(box_dir, cache_dir)
```

The `VmManager` gets a `rootfs_provider: Box<dyn RootfsProvider>` field, defaulting to auto-detect:
```rust
fn default_rootfs_provider() -> Box<dyn RootfsProvider> {
    let overlay = OverlayProvider;
    if overlay.is_available() {
        Box::new(overlay)
    } else {
        Box::new(CopyProvider)
    }
}
```

### 7. Cleanup on `stop()`

When a box stops, `VmManager::stop()` calls `rootfs_provider.cleanup(box_dir)` to unmount the overlay and remove upper/work dirs. This is already the natural place — the current code does `remove_dir_all(box_dir)` on stop.

## macOS Support

macOS does not have overlayfs. Non-snapshot MicroVMs now default to the
guest-native path described in the newer design: new OCI layers are assembled
directly into raw ext4 and cloned with `clonefile(2)` without creating an APFS
DiskImage mount. The directory-transport compatibility provider uses a
case-sensitive APFS staging filesystem only for snapshots and explicit legacy
operation.
Persistent restarts reuse that raw file and do not return to a mounted directory
transport. Clean stopped diff, export,
and commit operations attach it read-only to a restricted maintenance MicroVM,
so those workflows also avoid a macOS filesystem mount. Stopped legacy APFS
boxes can enter a versioned, resumable conversion transaction; the sparse image
is detached and retained as rollback evidence until a later explicit
garbage-collection policy.

## Performance Target

| Scenario | Current | With Overlay |
|----------|---------|-------------|
| Cold start (no cache) | ~3s (pull + extract + copy) | ~3s (pull + extract, no copy) |
| Warm start (cache hit) | ~1.5s (500MB copy) | <100ms (overlay mount) |
| Box stop cleanup | ~200ms (rm -rf rootfs) | ~50ms (umount + rm upper) |
| Disk usage (10 boxes, same image) | 5GB (10 × 500MB) | 500MB + deltas |

## Implementation Order

1. `rootfs/provider.rs` — trait + `CopyProvider` (extract current behavior)
2. `rootfs/overlay.rs` — `overlay_mount` / `overlay_unmount` / `check_overlay_support`
3. `rootfs/provider.rs` — `OverlayProvider` implementation
4. `vm/mod.rs` — integrate `RootfsProvider` into `VmManager` + `prepare_layout()`
5. `vm/mod.rs` — call `cleanup()` in `stop()`
6. Tests: unit tests for overlay mount/unmount, integration test for full boot cycle
7. CLI: `a3s-box info` shows rootfs provider type

## Test Plan

- Unit: `CopyProvider` prepare/cleanup
- Unit: `OverlayProvider` prepare/cleanup (Linux only, skip on macOS)
- Unit: `check_overlay_support()` detection
- Unit: `overlay_mount` / `overlay_unmount` with tempdir
- Integration: boot box with overlay → exec command → stop → verify cleanup
- Integration: two boxes from same image share lower layer, verify disk savings
- Regression: all existing 1504 tests pass unchanged (CopyProvider is default fallback)
