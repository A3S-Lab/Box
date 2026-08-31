//! Deterministic, streaming, pure-Rust ext4 image builder — plus a
//! verification-grade reader.
//!
//! Identical builder calls with identical [`Options`] produce
//! byte-identical images: UUID, htree hash seed, and every timestamp are
//! explicit inputs, and the crate never consults a clock or RNG. Output
//! streams through a [`RegionSink`]: every byte of the image is emitted
//! exactly once and is final — all metadata immediately at
//! [`Layout::writer`], file data behind it at ascending offsets.
//!
//! # Example
//!
//! Build a small image in memory, then read it back:
//!
//! ```
//! use mkext4::sink::VecSink;
//! use mkext4::{FsBuilder, Meta, Options, ROOT};
//!
//! # fn main() -> mkext4::Result<()> {
//! let epoch = 1_704_067_200;
//! let mut opts = Options::new(16 << 20, [0x42; 16], epoch);
//! opts.label = Some("demo".into());
//! let mut b = FsBuilder::new(opts)?;
//! let etc = b.mkdir(ROOT, "etc", Meta::new(0o755, 0, 0, (epoch, 0)))?;
//! let f = b.file(etc, "hostname", Meta::new(0o644, 0, 0, (epoch, 0)), 5)?;
//!
//! let layout = b.seal()?; // the complete physical layout is frozen here
//! let mut sink = VecSink::default();
//! let mut w = layout.writer(&mut sink)?;
//! w.fill(f, &mut &b"husky"[..])?;
//! w.finish()?;
//!
//! // sink.buf now holds a complete ext4 image (e2fsck-clean, mountable).
//! let fs = mkext4::reader::Fs::open(&sink.buf[..])?;
//! assert_eq!(fs.read_file(fs.resolve("/etc/hostname")?)?, b"husky");
//! # Ok(()) }
//! ```
//!
//! Layer map (bottom-up):
//! - [`csum`] / [`dirhash`] — ext4's crc32c conventions and the half_md4
//!   directory hash. Zero-allocation, append-style folds over borrowed
//!   slices; verified against byte vectors extracted from real `mke2fs`
//!   images.
//! - [`spec`] — on-disk structures with byte-exact `decode`/`encode` into
//!   caller-provided buffers.
//! - [`reader`] — walks and verifies complete filesystems (the
//!   differential oracle against `mke2fs`, and the round-trip check for
//!   the writer).

pub mod build;
pub mod csum;
pub mod dirhash;
pub mod reader;
pub mod sink;
pub mod spec;

pub(crate) mod layout;

pub use build::{
    Features, FsBuilder, InodeCount, InodeHandle, Layout, Meta, Options, SparseSeg, SpecialKind,
    ROOT,
};
pub use sink::RegionSink;

mod le;

/// Errors produced by this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Underlying I/O failure from a source or sink.
    Io(std::io::Error),
    /// The bytes being read do not form a valid structure.
    Corrupt {
        /// What was being decoded.
        what: &'static str,
        /// Why it was rejected.
        why: String,
    },
    /// Structurally valid, but uses a feature this crate does not handle.
    Unsupported(String),
    /// Invalid input to the builder/writer API (bad name, bad handle,
    /// capacity exceeded, protocol misuse like double-fill).
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Corrupt { what, why } => write!(f, "corrupt {what}: {why}"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Invalid(what) => write!(f, "invalid: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn corrupt(what: &'static str, why: impl Into<String>) -> Error {
    Error::Corrupt {
        what,
        why: why.into(),
    }
}
