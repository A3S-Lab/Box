# A3S mkext4 patch inventory

This directory vendors `mkext4` 0.0.3 from upstream commit
`645ba8f39e0a935511e233874f7217bcb6e0e4d8`.

A3S changes the namespace input boundary in `src/build/mod.rs`:

- directory entry names accept `AsRef<[u8]>` instead of `&str`;
- symbolic-link targets accept `AsRef<[u8]>` instead of `&str`;
- validation, duplicate detection, hashing, layout, and emitted ext4 bytes
  continue to operate on the same byte slices as upstream.

Linux filesystems define both values as non-NUL byte strings. This patch lets
the macOS staging adapter restore OCI path bytes without lossy UTF-8 conversion.
The integration test in `tests/writer.rs` verifies raw names and link targets
through mkext4's independent reader.

Existing test call sites drop obsolete borrows introduced by the generic byte
input. Those mechanical changes do not alter test coverage or writer behavior.
