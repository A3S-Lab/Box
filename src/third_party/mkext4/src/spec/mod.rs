//! On-disk ext4 structures: byte-exact `decode` / `encode`.
//!
//! Every structure decodes from and encodes into caller-provided byte
//! slices — no allocation, no `unsafe`, all field offsets explicit and
//! greppable. `encode(decode(bytes)) == bytes` holds for
//! every structure on real mke2fs output (proven by unit tests against
//! `testdata/vectors/`), which guarantees the model captures every
//! nonzero byte these tools produce.
//!
//! Decoding is *tolerant* of fields this crate does not model (reserved
//! and feature-specific regions decode-ignore, encode-zero), so the
//! reader can open foreign images; the byte-exactness guarantee applies
//! to the feature set this crate's writer emits.

pub mod consts;
pub mod dirent;
pub mod extent;
pub mod group_desc;
pub mod htree;
pub mod inode;
pub mod journal;
pub mod superblock;
pub mod xattr;

pub use consts::*;
pub use dirent::{DirEntryRef, DirentIter};
pub use extent::{Extent, ExtentHeader, ExtentIdx};
pub use group_desc::GroupDesc;
pub use htree::{DxEntry, DxInfo, DxNodeView, DxRootView};
pub use inode::Inode;
pub use journal::JournalSuperblock;
pub use superblock::Superblock;
pub use xattr::{ibody_entries, XattrBlockView, XattrEntry};
