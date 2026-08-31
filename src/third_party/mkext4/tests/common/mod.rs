//! Helpers shared by the integration tests.
#![allow(dead_code)] // each test binary uses a different subset

use std::io::Read;
use std::path::{Path, PathBuf};

/// Timestamp used across test namespaces (2024-01-01T00:00:00Z).
pub const EPOCH: i64 = 1_704_067_200;

/// Deterministic pattern source: an LCG byte stream of a fixed length.
pub struct Pattern {
    remaining: u64,
    counter: u64,
}

impl Pattern {
    pub fn new(len: u64, seed: u64) -> Pattern {
        Pattern {
            remaining: len,
            counter: seed,
        }
    }
}

impl Read for Pattern {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.remaining).min(buf.len() as u64) as usize;
        for b in &mut buf[..n] {
            self.counter = self
                .counter
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1);
            *b = (self.counter >> 33) as u8;
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// The whole pattern stream as bytes.
pub fn pattern_bytes(len: u64, seed: u64) -> Vec<u8> {
    let mut v = Vec::new();
    Pattern::new(len, seed).read_to_end(&mut v).unwrap();
    v
}

/// `len` bytes of the pattern at `offset` without holding the whole
/// stream.
pub fn pattern_at(total: u64, seed: u64, offset: u64, len: usize) -> Vec<u8> {
    let mut p = Pattern::new(total, seed);
    let mut skip = vec![0u8; 1 << 20];
    let mut remaining = offset;
    while remaining > 0 {
        let n = remaining.min(skip.len() as u64) as usize;
        p.read_exact(&mut skip[..n]).unwrap();
        remaining -= n as u64;
    }
    let mut out = vec![0u8; len];
    p.read_exact(&mut out).unwrap();
    out
}

/// Locate the pinned e2fsprogs toolchain: `E2FSPROGS_SBIN`, the Homebrew
/// keg, or `PATH`. `None` = skip oracle-dependent assertions.
pub fn e2fsprogs_sbin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("E2FSPROGS_SBIN") {
        return Some(PathBuf::from(p));
    }
    for cand in [
        "/opt/homebrew/opt/e2fsprogs/sbin",
        "/usr/local/opt/e2fsprogs/sbin",
    ] {
        if Path::new(cand).join("e2fsck").exists() {
            return Some(PathBuf::from(cand));
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join("e2fsck").exists())
}

/// `e2fsck -fn` must exit 0 with no complaints — under the pinned
/// e2fsprogs AND, when `E2FSCK_VINTAGE` names an older e2fsck binary,
/// under that vintage too (published images must be clean across fsck
/// generations). Skips (with a note) when e2fsprogs is unavailable.
/// `dir` is the test binary's tmpdir.
pub fn assert_fsck_clean(dir: &Path, image: &[u8], tag: &str) {
    let Some(sbin) = e2fsprogs_sbin() else {
        eprintln!("SKIP fsck gate: e2fsprogs not found");
        return;
    };
    std::fs::create_dir_all(dir).unwrap();
    let img = dir.join(format!("{tag}.img"));
    std::fs::write(&img, image).unwrap();
    let mut fscks = vec![sbin.join("e2fsck")];
    if let Ok(vintage) = std::env::var("E2FSCK_VINTAGE") {
        fscks.push(PathBuf::from(vintage));
    }
    for fsck in fscks {
        let out = std::process::Command::new(&fsck)
            .arg("-fn")
            .arg(&img)
            .output()
            .unwrap_or_else(|e| panic!("running {}: {e}", fsck.display()));
        assert!(
            out.status.success(),
            "{tag}: {} -fn failed:\n{}{}",
            fsck.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = std::fs::remove_file(&img);
}
