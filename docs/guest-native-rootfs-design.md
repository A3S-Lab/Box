# Guest-Native Rootfs Design

## Decision

A3S Box should present MicroVM root filesystems as guest-native block devices.
The target artifact is an explicitly raw ext4 filesystem image attached through
libkrun virtio-blk and mounted as `/dev/vda` by libkrun's root-disk remount path.

The per-box case-sensitive APFS sparse image remains a compatibility transport
during migration. It is not the target storage model.

An experimental macOS provider now implements mount-free construction, the
running ownership handoff, the read-only stopped archive path, and stopped
legacy migration. New OCI generations are assembled directly into a validated
raw ext4 artifact; APFS DiskImages are attached only while converting an
existing legacy writable generation and are synchronously detached before the
VMM starts. Persistent boxes restart directly from that guest-written disk;
clean stopped boxes use a restricted maintenance MicroVM. This remains an
opt-in path, not yet the default.

## Problem

The current default macOS compatibility provider keeps one case-sensitive APFS
sparse image attached for every running box and exports its directory through
virtio-fs. This solves
Linux case-sensitivity on a case-insensitive host, but it also makes a guest
implementation detail visible as one macOS disk device per box. A compose
application with many services therefore creates many `A3SRootfs` devices.

This is not primarily an eject bug. The mounts are required for as long as
virtio-fs serves those directories. Better names, Finder flags, or more eager
cleanup can reduce symptoms, but they cannot remove the one-host-mount-per-box
runtime invariant.

## First Principles

The design starts from these invariants:

1. A Linux guest rootfs is guest state. It should not require a mounted host
   filesystem while the guest owns it.
2. OCI content is immutable and content-addressed. Per-box writable state must
   branch from it without copying every byte eagerly.
3. The host may construct an artifact before boot, but there must be one clear
   ownership handoff. Host and guest must never mutate the same rootfs
   concurrently.
4. Filesystem type and image format are trusted configuration, not data to
   probe after an untrusted guest has written to the image.
5. Dynamic machine configuration is not image content. DNS, hostname, hosts,
   runtime metadata, secrets, and mounts should cross the control boundary and
   be applied by guest-init.
6. Product releases must contain every required runtime primitive. A random
   `mke2fs` found on `PATH` is not a production dependency contract.
7. A failed build or migration must leave either the old complete generation or
   the new complete generation. Partial disk images are never bootable state.

## Target Architecture

```text
OCI layers
    |
    v
immutable ext4 base image (content-addressed cache)
    |
    | clonefile/reflink or an explicit copy fallback
    v
per-box rootfs.ext4 (raw, sparse, writable)
    |
    | libkrun virtio-blk, format fixed to raw
    v
/dev/vda mounted as / inside the Linux guest
    |
    +-- guest-init applies dynamic configuration
    +-- pre-opened private status fd publishes the exact workload exit
    +-- one-shot baseline fd publishes the pristine guest-visible tree
    +-- guest archive/control protocol serves diff, export, and commit

stopped clean rootfs.ext4
    |
    | exclusive read-only auxiliary virtio-blk
    v
trusted one-shot maintenance MicroVM
    +-- current release guest-init on an ephemeral virtio-fs root
    +-- ext4 mounted ro,noload at /mnt/a3s-rootfs
    +-- heartbeat, archive, and shutdown protocol only
```

On APFS, cloning a raw sparse file provides host-level copy-on-write without
creating a mounted macOS volume. Linux can use reflink where supported. The
artifact remains raw in both cases, so the VMM contract does not vary with the
host storage optimization.

## Ownership State Machine

```text
LogicalAssembly | LegacyStagingDirectory
    -> Finalizing
    -> BlockArtifactReady
    -> GuestOwned
    -> GuestQuiescing
    -> Quiescent
    -> GuestOwned | MaintenanceOwned | Deleted
```

- `LogicalAssembly`: new OCI layers are resolved in a raw-byte Linux namespace;
  regular-file payloads use opaque temporary spool names rather than guest path
  names on the host.
- `LegacyStagingDirectory`: host code may inspect an attached legacy APFS
  generation only inside the resumable migration transaction.
- `Finalizing`: the provider creates a temporary block artifact and validates
  it. No guest may start.
- `BlockArtifactReady`: the artifact was atomically published; the staging
  mount can be detached.
- `GuestOwned`: libkrun has the only writable open handle. Host filesystem
  mutation is forbidden.
- `GuestQuiescing`: the host has signalled the workload over private guest
  control. PID 1 owns the final sync, read-only remount, and handoff reply; the
  host still must not parse or mutate the disk.
- `Quiescent`: PID 1 has remounted the block root read-only, published a
  `rootfs_quiesced` acknowledgement through its pre-opened terminal descriptor,
  and exited. The raw artifact is now the durable restart generation.
- `MaintenanceOwned`: a dedicated maintenance boot owns a clean disk read-only
  for offline diff, export, and commit. It runs no user workload, exposes no
  network or user attachment, and macOS never mounts the disk. Repair,
  migration, snapshots, and arbitrary file inspection are separate future
  capabilities and are not implied by archive access.

The runtime lifecycle lock must serialize every rootfs ownership transition.

## Runtime Contract

`InstanceSpec` carries a typed `RootfsSource`:

```rust
pub enum RootfsSource {
    Directory { path: PathBuf },
    Ext4Disk { path: PathBuf, read_only: bool },
}
```

The directory variant preserves the existing Linux overlay, Windows, and
migration paths. The ext4 variant has these fixed semantics:

- the file is a raw filesystem image, not a partitioned disk;
- it is the first libkrun block device and therefore `/dev/vda`;
- libkrun is configured with filesystem type `ext4`;
- the image format is passed explicitly as raw;
- an empty, missing, or non-regular artifact is rejected before entering
  libkrun.

`RootfsProvider::prepare_oci_for_boot` is the mount-free ownership boundary for
new guest-native generations. Directory providers decline it and retain their
existing staging flow. `RootfsProvider::finalize_for_boot` remains the boundary
for directory providers and legacy migration: it runs after all permitted
host-side mutations and before the VMM starts. The experimental macOS block
provider returns `Ext4Disk` from either boundary.

For stopped inspection, `InstanceSpec::block_devices` carries typed auxiliary
raw disks. The maintenance spec has a tiny trusted directory root containing
the current static guest-init and exactly one read-only auxiliary disk. The
shim validates IDs and paths, fixes the format to raw, and keeps a non-blocking
exclusive `flock` for every raw root or auxiliary disk for its entire lifetime.
This second ownership fence catches stale state records and orphan processes in
addition to the CLI lifecycle lock.

The standard Unix shim suite fault-injects both sides of that fence in separate
processes. A maintenance-shaped owner blocks a writable run spec, a
run-shaped owner blocks a read-only maintenance spec, and an ungraceful owner
process kill must release the kernel lock before the opposite role can acquire
the same generation. This test exercises the production validation and lock
path without requiring a hypervisor; the physical HVF gate separately proves
that normal and maintenance boots use those exact spec shapes.

### One-Shot Boot Control

MicroVM launch data is carried in a versioned `a3s.box.guest-boot.v1` bundle,
not written into the rootfs. The runtime exposes one private virtio-fs tag at
`/run/a3s-box/boot`; guest-init mounts it read-only, validates and reads
`config.json`, and unmounts it before the workload starts. The host removes the
payload after guest readiness, so a later privileged remount sees an empty
share.

The bundle contains the workload exec configuration, workload environment, and
the rendered hostname, resolver, and hosts state. Its schema, total bytes,
argument count, environment count, duplicate names, NUL bytes, hostname, and
host-file sizes are bounded on both sides of the trust boundary. User volumes,
OCI `VOLUME` entries, and tmpfs mounts may not cover `/run/a3s-box` or an
ancestor of it.

Sandbox intentionally retains the legacy rootfs-file transport. Its OCI
runtime already owns a directory root and installs mounts before PID 1, so this
migration does not change its filesystem contract.

### Root Transport Selection

Block-root and directory-root boots must be distinguished explicitly. For a
block root, libkrun's `init.krun` starts on a private empty virtio-fs bootstrap,
mounts `/dev/vda`, and switches root before executing A3S guest-init. The empty
bootstrap still owns the conventional `/dev/root` virtio-fs tag. Guest-init
therefore treats `KRUN_BLOCK_ROOT_DEVICE=/dev/vda` as an already-completed root
handoff and never probes or pivots to `/dev/root` in that mode. Directory roots
retain the legacy virtio-fs pivot.

This distinction is required for correctness, not optimization: probing the
tag after a block-root handoff would switch the guest back into libkrun's empty
bootstrap filesystem.

### Terminal Status Control

The exact workload exit code is runtime state, not rootfs content. New MicroVM
specs expose a separate writable `a3s-terminal` share containing one bounded
status file. Before any workload process starts, guest-init mounts the share,
opens the plain file with `O_NOFOLLOW | O_CLOEXEC`, and lazy-detaches the mount.
PID 1 retains only that file descriptor and writes a versioned
`a3s.box.guest-terminal.v1` result when the workload exits. For a block root,
PID 1 then calls `syncfs`, remounts `/` read-only, calls `syncfs` again, and
rewrites that result with `rootfs_quiesced=true` before it exits.

The workload cannot see the share and does not inherit the descriptor across
`exec`. The host trusts the bounded status document rather than libkrun's shim
exit code; a staged but empty or invalid status fails closed, and a shim exit of
zero is never substituted for a missing guest result. Legacy directory and
Sandbox paths retain their existing rootfs marker fallback. Managed stop first
delivers the configured signal to the workload through this private control
plane. On timeout it asks guest-init to SIGKILL the workload and still grants
PID 1 a bounded finalization window. A host-side shim kill is only the final
fallback and cannot produce a clean block-root acknowledgement.

### Guest-Owned Diff Baseline

The pristine `diff` baseline follows the same ownership rule as terminal
status. On the first guest-native generation only, the runtime pre-creates a
private `baseline.json` in the terminal-control share. Guest-init opens it with
`O_NOFOLLOW | O_CLOEXEC` before lazy-detaching that share, then walks `/` after
dynamic configuration and mounts are installed but before any workload or
sidecar starts.

The walk uses the same nested-mount and runtime-state exclusions as live
archive export. It therefore describes the Linux tree that the workload can
mutate without importing procfs, tmpfs, workspace, user volumes, Secrets, or
`/run/a3s-box` control state. The guest emits the versioned
`a3s.box.guest-diff-baseline.v1` envelope through its retained descriptor. The
host bounds the payload to 64 MiB and 1,000,000 canonical absolute entries,
rejects runtime-owned paths, and atomically publishes the first validated
generation as the existing `rootfs_snapshot.json` format. It then unlinks the
one-shot handoff; failure to remove that redundant source cannot invalidate the
already durable canonical baseline.

Once that canonical baseline exists, later boots remove any stale handoff file
and do not rescan the guest rootfs. Directory providers retain the compatibility
host-side walk.

## Artifact Construction

The ext4 builder must:

1. take either the resolved logical OCI namespace or an explicitly migrated
   legacy staging tree as input;
2. preserve Linux paths, modes, ownership, timestamps, hard links, symlinks,
   extended attributes, and sparse files;
3. derive logical capacity from `resources.disk_mb` and fail before boot when
   the tree cannot fit;
4. write to a sibling temporary file, sync it, validate it, and rename it into
   place atomically;
5. create a sparse host file rather than eagerly writing its full capacity;
6. use a pinned, release-owned implementation with identical behavior on CI
   and user machines.

The implementation can be an audited in-process writer or a helper shipped in
the release archive. Tool discovery from the user's shell is acceptable only
for development diagnostics, never as the default production path.

The current experimental writer pins `mkext4` exactly at `0.0.3` and compiles
it into the runtime. The source is vendored from upstream commit
`645ba8f39e0a935511e233874f7217bcb6e0e4d8`; the A3S patch changes only
directory-entry and symlink-target inputs from UTF-8 strings to byte slices.
Runtime uses a direct versioned path dependency, rather than a workspace-only
Cargo patch, so Git consumers compile this same audited implementation.
The writer's validation, hashing, layout, and streaming algorithms remain
upstream. Its independent reader verifies that arbitrary Linux filename and
symlink-target bytes round-trip exactly.

A3S owns the tree adapter, capacity policy, OCI ownership replay, sparse-file
discovery, xattr filtering, structural validation, and generation-directory
publication. The adapter does not use the crate's example tree walker.
Direct construction authenticates the exact compressed layer snapshot before
decoding it, reclaims superseded payload spools as whiteouts and replacements
are applied, and records aligned zero runs as sparse ext4 holes without relying
on host sparse-file discovery.
Runtime-managed files use canonical metadata: guest-init is root owned, mode
`0755`, and timestamped at the filesystem epoch rather than inheriting metadata
from an image entry it replaced.

APFS cannot materialize every Linux filename byte sequence and normalizes some
distinct Unicode spellings. The compatibility extraction path therefore maps
non-ASCII path components, plus literal names in the private codec namespace,
to deterministic physical names. That codec and its OCI metadata manifest
remain necessary for directory transport and legacy migration. New
guest-native construction never encodes a guest path into a host pathname: it
tracks raw byte components, inode identity, symlink resolution, whiteouts,
opaque directories, xattrs, and metadata in memory, while regular payloads are
spooled under unrelated bounded host names. Directory providers still reject a
translated tree instead of exposing codec names through virtio-fs.

The byte-capable adapter uses builder identity
`mkext4/0.0.3+a3s-adapter-v3`, which invalidates older immutable cache entries.
Already-published v1 and v2 per-box disks remain valid resumable generations
because their ext4 bytes no longer depend on a host staging namespace.

## Experimental Handoff

On macOS, set `A3S_BOX_EXPERIMENTAL_GUEST_NATIVE_ROOTFS=1` to select the
experimental provider. Its current lifecycle is:

1. resolve verified OCI layers into a bounded raw-byte logical namespace,
   applying lower-layer-only whiteouts without creating a guest-named host tree;
2. derive an immutable identity from the resolved OCI manifest, Linux platform,
   exact guest-init SHA-256, ext4 contract, writer version, and capacity;
3. when artifact caching is enabled, reuse or atomically publish the matching
   sparse ext4 base under the bounded `rootfs-ext4-v1` cache namespace;
4. clone the cached base or atomically publish an uncached private generation at
   the box-local `rootfs-ext4-v1/rootfs.ext4`; neither form is mounted by macOS;
5. pass only the box-local raw disk to libkrun;
6. on clean stop, wait for guest PID 1 to flush and acknowledge the read-only
   root-disk handoff before tearing down host runtime state;
7. for a persistent box, reopen the exact raw generation on the next start
   without attaching APFS, pulling the image again, or reconstructing a host
   directory tree.

Cache entries have a separate `a3s.box.rootfs-ext4-cache.v1` manifest. Lookup
revalidates the exact identity, artifact schema, builder, capacity, UUID,
sparse-file integrity digest, and ext4 structure. Publication and cloning use
synced temporary directories followed by atomic rename. Pruning is LRU by
allocated sparse blocks and uses the configured rootfs entry and byte bounds.
An invalid derived entry is removed under the cache lock and rebuilt from the
resolved OCI tree; invalid bytes are never cloned into a box generation.
One cross-process cache lock currently serializes lookup, first publication,
cloning, and pruning; this keeps the initial protocol simple and crash-safe.

Cache misses now use the same direct OCI-layer-to-ext4 assembly path as
uncached construction. Cache hits clone the already validated raw base without
decoding or spooling any layer. Neither path creates or attaches an
`A3SRootfs` DiskImage. The only remaining APFS attach in the guest-native
provider is the explicit legacy migration window.

The provider supports persistent clean-stop restart and intentionally rejects
snapshot-backed boxes. A retained `rootfs-ext4-v1` generation selects the same
provider on later starts even when the experimental creation switch is absent;
falling back to an old APFS staging tree would lose guest data. Clean stopped
diff, export, and commit now run through the maintenance guest. Snapshot and
arbitrary file-access operations remain disabled for this provider. The
compatibility provider remains the default for new boxes and continues to serve
snapshots.

An unclean host or shim exit is a different valid state, not automatic
corruption. If the mutable disk carries exactly the builder's feature set plus
ext4's `RECOVER` bit, the host validates its regular-file identity, length,
manifest UUID, fixed geometry, journal identity, and primary-superblock
checksum without walking unreplayed filesystem metadata. It then delegates
journal replay to the guest kernel on the next read-write mount. Any other
feature, geometry, UUID, or checksum drift fails closed. A later clean guest
handoff clears `RECOVER` and returns the generation to full structural
verification.

## Dynamic Configuration

Most launch-specific mutations now cross the one-shot boot bundle and are
applied inside the guest. The remaining ownership work is explicit:

| Concern | Current or target owner |
| --- | --- |
| `/etc/resolv.conf` | guest-init from typed DNS configuration (implemented) |
| `/etc/hostname` | guest-init from the instance spec (implemented) |
| `/etc/hosts` and peer entries | guest-init from typed network endpoints (implemented) |
| volume and tmpfs targets | guest-init from validated mount specifications (implemented) |
| exact workload exit status | private pre-opened terminal-control fd (implemented) |
| stopped filesystem archive | trusted read-only maintenance guest (implemented) |
| runtime exec and environment | one-shot private boot-control bundle (implemented) |
| diff baseline | one-shot guest-side manifest before workload launch (implemented) |

Secrets must retain their existing transient delivery and zeroization rules;
they must not be baked into an immutable base image.

## Persistence, Commit, and Offline Operations

Persistent boxes keep `rootfs.ext4` as their authoritative writable state.
They must not reconstruct it from an old staging directory on restart.

Live commit, diff, and export use the guest-init archive stream with Linux
metadata over the exec channel; none requires a host-visible rootfs directory.
Stopped clean generations use the same archive representation through a
dedicated maintenance guest.

For a stopped box, A3S runs a narrowly scoped maintenance guest that:

- boots the current static A3S guest-init from a fresh ephemeral directory root,
  never the user-modifiable `/sbin/init` on the inspected disk;
- opens the exact raw root disk exclusively and read-only;
- mounts ext4 with `MS_RDONLY` and `noload`, and rejects a generation carrying
  the journal `RECOVER` bit instead of replaying it in an observational path;
- starts no user workload and exposes no network, workspace, volume, PTY,
  attestation, port-forward, Secret, sidecar, or arbitrary exec endpoint;
- serves only heartbeat, archive, and clean-shutdown requests over the private
  exec control channel;
- shuts down before the lifecycle lock is released.

This avoids adding an ext4 kernel extension, FUSE daemon, or visible DiskImages
mount to macOS. A bounded idle lifetime and shim-held raw-disk lock provide
orphan and concurrent-owner fences. The physical Apple Silicon/HVF gate proves
stopped diff, export, and commit, verifies the disk mtime and terminal status do
not change, checks that ext4 remains clean, restarts the same disk afterward,
and confirms no A3S host mount remains.

## Cache and Snapshot Model

The immutable cache key must include at least:

- resolved OCI manifest digest;
- platform and architecture;
- guest-init artifact identity;
- ext4 layout/schema version;
- builder version and feature set.

Per-box disks branch from an immutable base with clonefile or reflink. A cache
entry is never opened writable. Snapshot identity must bind the root disk
generation to the matching memory/VMM generation; restoring either half alone
is invalid.

The mutable image reference is diagnostic input, not cache authority. A moved
tag resolves to a different manifest digest and therefore a different base.
Dynamic volumes, tmpfs, process data, DNS, hostname, and hosts state are applied
by guest-init and are intentionally absent from the immutable key and disk.

Disk format or ext4 layout changes intentionally invalidate the cache. Cache
schema changes must not silently reinterpret old bytes.

## Migration

Migration is per box and only while stopped:

1. acquire the lifecycle lock and verify no live shim owns the box;
2. attach the legacy APFS image only for the migration window;
3. build and validate `rootfs.ext4.tmp` from the exact guest-visible tree;
4. atomically publish `rootfs.ext4` plus a versioned artifact manifest;
5. detach the legacy image;
6. retain the old sparse image until one successful boot and clean stop;
7. remove it through an explicit, recoverable garbage-collection step.

Migration must be resumable. Presence of a temporary file is not evidence of a
completed generation.

The current opt-in implementation persists
`rootfs-migration-v1.json` before attaching an existing legacy image. Its
states are `building`, `artifact_ready`, and `clean_stop_verified`.
An atomically published ext4 directory or the migration manifest selects the
guest-native provider on every later process invocation, independent of the
environment variable. A process failure before publication leaves the APFS
source authoritative and the next attempt removes only A3S-named staging
entries before rebuilding. A failure after publication reconciles the complete
validated artifact, strictly detaches any legacy mount, and resumes from ext4.
The immutable OCI artifact cache is never used for conversion because it cannot
represent writes made by the legacy guest.

The source sparse image remains a plain, detached rollback generation after the
guest completes one successful boot and acknowledged clean stop. That handoff
advances the transaction to `clean_stop_verified`; deletion remains a separate
future garbage-collection policy so a migration never consumes its rollback
window implicitly.

## Rejected End States

- **Hide or rename every APFS volume:** cosmetic only; N boxes still require N
  host mounts.
- **Detach after boot while keeping virtio-fs root:** impossible because the VMM
  continues serving the mounted directory.
- **One shared writable APFS volume:** reduces visible devices but creates a
  shared corruption, quota, lifecycle, and cleanup boundary across boxes.
- **Require Docker, FUSE, or a system ext4 driver:** adds a larger privileged
  dependency than the runtime itself needs.
- **Probe raw versus qcow2 at boot:** guest-controlled bytes must not choose the
  host parser.

## Delivery Phases

### Phase 0: Contract seam

- [x] Add typed directory and raw-ext4 rootfs sources.
- [x] Preserve deserialization of legacy `rootfs_path` specs.
- [x] Configure libkrun virtio-blk and root-disk remount explicitly.
- [x] Validate the artifact before entering libkrun.
- [x] Add a final provider boundary after host-side preparation.

### Phase 1: Immutable block cache

- [x] Select and exactly pin the in-process ext4 writer for the experimental path.
- [x] Add deterministic tree adaptation, sparse output, structural validation, and atomic generation publication.
- [x] Add raw-byte filename and symlink-target support or replace the writer.
- [x] Assemble OCI layers directly into ext4 without an APFS staging namespace.
- [x] Build and reuse a deterministic immutable ext4 base cache from OCI content.
- [ ] Validate metadata fidelity and filesystem integrity on Linux CI.
- [x] Add atomic publication and cache-version tests for the experimental artifact.

### Phase 2: Ephemeral MicroVM path

- [x] Move exec, environment, DNS, hostname, and hosts into a bounded guest-init bundle.
- [x] Consume the bundle before workload mounts and remove it after guest readiness.
- [x] Move exact exit status and terminal-generation fencing out of the host tree.
- [x] Move diff baseline ownership out of the host tree.
- [x] Hand an ephemeral per-box raw disk to libkrun without creating an APFS construction mount.
- [x] Clone an immutable base image into one per-box raw disk.
- [x] Run real macOS success and nonzero-exit smoke tests with zero `A3SRootfs` runtime mounts.
- [x] Add clean restart and forced-crash recovery to the physical macOS/HVF integration gate.
- [x] Keep an explicit compatibility switch during rollout.

### Phase 3: Persistent and maintenance path

- [x] Reuse the guest-written raw disk across clean-stop restarts.
- [x] Make managed stop signal the workload in-guest and verify a read-only rootfs handoff.
- [x] Route live diff, export, and commit through guest archive control.
- [x] Recover a journaled generation after a host or shim crash inside the guest.
- [x] Route stopped diff, export, and commit through maintenance control.
- [x] Add the stopped-box read-only maintenance guest.
- [ ] Add explicitly scoped stopped file inspection if the product requires it.
- [x] Prove maintenance/run exclusive ownership under fault injection.

### Phase 4: Migration and default switch

- [x] Implement resumable APFS-to-ext4 migration.
- [ ] Migrate cache and snapshot formats with versioned manifests.
- [ ] Make guest-native ext4 the macOS default.
- [ ] Remove legacy sparse images only after verified rollback windows.

## Release Gates

The default must not switch until CI and host integration prove:

- no macOS DiskImages attachment remains while an ext4-backed box is running;
- ext4 images pass a pinned independent filesystem check;
- OCI ownership, modes, links, xattrs, case-sensitive names, arbitrary filename
  bytes, and arbitrary symlink-target bytes survive;
- persistent writes survive restart and host-process crashes;
- live and stopped commit produce equivalent OCI content;
- cleanup leaves no shim, block image handle, socket, or temporary artifact;
- concurrent boxes never share a writable rootfs generation;
- legacy boxes either migrate successfully or continue on the compatibility
  path without data loss.
