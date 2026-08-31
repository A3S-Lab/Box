//! Kernel oracle: loop-mount a generated image and diff the mounted tree
//! against the declared namespace — types, modes, owners, timestamps,
//! nlink/hardlink identity, symlink targets, device numbers, xattrs,
//! content bytes, sparse holes, and htree lookups through the kernel.
//!
//! Requires root + Linux; gated on MKEXT4_MOUNT=1. CI builds the
//! test binary unprivileged and runs it under sudo.

#![cfg(target_os = "linux")]

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use mkext4::sink::FileSink;
use mkext4::{FsBuilder, Meta, Options, SparseSeg, SpecialKind, ROOT};

mod common;
use common::{pattern_bytes, Pattern, EPOCH};

fn meta(mode: u16, uid: u32, gid: u32) -> Meta {
    Meta::new(mode, uid, gid, (EPOCH, 123_456_789))
}

fn sh(cmd: &str) -> Result<String, String> {
    let out = Command::new("sh").arg("-c").arg(cmd).output().unwrap();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "`{cmd}` failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

#[test]
fn kernel_mount_oracle() {
    if std::env::var("MKEXT4_MOUNT").as_deref() != Ok("1") {
        eprintln!("SKIP: set MKEXT4_MOUNT=1 and run as root");
        return;
    }
    assert_eq!(effective_uid(), 0, "must run as root");

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let img = dir.join("mount_oracle.img");
    let mnt = dir.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();

    // --- build ------------------------------------------------------------
    let size = 1u64 << 30;
    let mut opts = Options::new(size, *b"mkext4-mounttest", EPOCH);
    opts.hash_seed = [1, 2, 3, 4];
    opts.label = Some("mnttest".into());
    opts.inodes = mkext4::InodeCount::Exact(150_000);
    let mut b = FsBuilder::new(opts).unwrap();

    let usr = b.mkdir(ROOT, "usr", meta(0o755, 0, 0)).unwrap();
    let f = b
        .file(usr, "file", meta(0o640, 1000, 1000), 100_000)
        .unwrap();
    b.hardlink(usr, "link", f).unwrap();
    b.file(usr, "empty", meta(0o600, 0, 0), 0).unwrap();
    b.symlink(ROOT, "fastlink", "usr/file", meta(0o777, 0, 0))
        .unwrap();
    b.symlink(ROOT, "slowlink", "long/".repeat(30), meta(0o777, 0, 0))
        .unwrap();
    b.mknod(
        ROOT,
        "null",
        meta(0o666, 0, 0),
        SpecialKind::Char { major: 1, minor: 3 },
    )
    .unwrap();
    b.mknod(
        ROOT,
        "bigdev",
        meta(0o600, 0, 0),
        SpecialKind::Block {
            major: 259,
            minor: 70000,
        },
    )
    .unwrap();
    b.mknod(ROOT, "fifo", meta(0o644, 0, 0), SpecialKind::Fifo)
        .unwrap();
    let xf = b.file(ROOT, "xattrs", meta(0o644, 0, 0), 5).unwrap();
    b.set_xattr(xf, "user.small", b"hello").unwrap();
    b.set_xattr(xf, "user.big", &vec![0x41u8; 500]).unwrap();
    b.set_xattr(xf, "security.selinux", b"system_u:object_r:etc_t:s0")
        .unwrap();
    // 100k entries in one directory: the htree proof, looked up through
    // the kernel below.
    let htree = b.mkdir(ROOT, "htree", meta(0o755, 0, 0)).unwrap();
    for i in 0..100_000 {
        b.file(htree, format!("entry_{i:06}_padpad"), meta(0o644, 0, 0), 0)
            .unwrap();
    }
    let sp = b
        .file_sparse(
            ROOT,
            "sparse",
            meta(0o644, 0, 0),
            &[
                SparseSeg::Data(4096),
                SparseSeg::Hole(64 << 20),
                SparseSeg::Data(3000),
            ],
        )
        .unwrap();

    let layout = b.seal().unwrap();
    let mut sink = FileSink::create(&img, layout.image_len()).unwrap();
    let mut w = layout.writer(&mut sink).unwrap();
    w.fill(f, &mut Pattern::new(100_000, 1)).unwrap();
    w.fill(xf, &mut Pattern::new(5, 2)).unwrap();
    w.fill(sp, &mut Pattern::new(4096 + 3000, 3)).unwrap();
    w.finish().unwrap();
    drop(sink);

    // --- mount ------------------------------------------------------------
    sh(&format!(
        "mount -o loop,ro {} {}",
        img.display(),
        mnt.display()
    ))
    .expect("loop mount");
    // Unmount even on panic.
    struct Umount(std::path::PathBuf);
    impl Drop for Umount {
        fn drop(&mut self) {
            let _ = sh(&format!("umount {}", self.0.display()));
        }
    }
    let _guard = Umount(mnt.clone());

    // --- diff ---------------------------------------------------------------
    // Regular file: content, mode, owner, mtime (ns), nlink.
    let p = mnt.join("usr/file");
    let m = std::fs::metadata(&p).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), pattern_bytes(100_000, 1));
    assert_eq!(m.permissions().mode() & 0o7777, 0o640);
    assert_eq!((m.uid(), m.gid()), (1000, 1000));
    assert_eq!(m.mtime(), EPOCH);
    assert_eq!(m.mtime_nsec(), 123_456_789);
    assert_eq!(m.nlink(), 2);
    assert_eq!(
        m.ino(),
        std::fs::metadata(mnt.join("usr/link")).unwrap().ino(),
        "hardlink shares the inode"
    );

    // Symlinks.
    assert_eq!(
        std::fs::read_link(mnt.join("fastlink")).unwrap(),
        Path::new("usr/file")
    );
    assert_eq!(
        std::fs::read_link(mnt.join("slowlink"))
            .unwrap()
            .as_os_str()
            .len(),
        150
    );

    // Devices (rdev encoding as the kernel sees it).
    let null = std::fs::metadata(mnt.join("null")).unwrap();
    assert!(null.file_type().is_char_device());
    assert_eq!(null.rdev(), makedev(1, 3));
    let bigdev = std::fs::metadata(mnt.join("bigdev")).unwrap();
    assert!(bigdev.file_type().is_block_device());
    assert_eq!(bigdev.rdev(), makedev(259, 70000));
    assert!(std::fs::metadata(mnt.join("fifo"))
        .unwrap()
        .file_type()
        .is_fifo());

    // xattrs through the kernel.
    let x = mnt.join("xattrs");
    assert_eq!(getfattr(&x, "user.small"), b"hello");
    assert_eq!(getfattr(&x, "user.big").len(), 500);
    assert_eq!(
        getfattr(&x, "security.selinux"),
        b"system_u:object_r:etc_t:s0"
    );

    // htree proof: kernel-side hashed lookups of a deterministic 1k-name
    // sample across the 100k-entry directory, plus a full readdir count.
    for i in (0..100_000).step_by(100) {
        let p = mnt.join(format!("htree/entry_{i:06}_padpad"));
        assert!(p.symlink_metadata().is_ok(), "{}", p.display());
    }
    assert_eq!(
        std::fs::read_dir(mnt.join("htree")).unwrap().count(),
        100_000
    );

    // Sparse: size, hole reads as zeros, block count reflects holes.
    let sp_path = mnt.join("sparse");
    let m = std::fs::metadata(&sp_path).unwrap();
    assert_eq!(m.len(), 4096 + (64 << 20) + 3000);
    assert!(m.blocks() < 100, "holes must not be allocated");
    let mut content = std::fs::read(&sp_path).unwrap();
    let want = pattern_bytes(4096 + 3000, 3);
    assert_eq!(&content[..4096], &want[..4096]);
    assert!(content[4096..4096 + (64 << 20)].iter().all(|&b| b == 0));
    assert_eq!(&content[4096 + (64 << 20)..], &want[4096..]);
    content.clear();

    // Whole-tree walk: every path readable, no kernel errors.
    fn walk(p: &Path) -> usize {
        let mut n = 0;
        for e in std::fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            n += 1;
            if e.file_type().unwrap().is_dir() {
                n += walk(&e.path());
            }
        }
        n
    }
    assert!(walk(&mnt) > 100_000);

    let _ = std::fs::remove_file(&img);

    fn makedev(major: u64, minor: u64) -> u64 {
        // Linux dev_t encoding (glibc makedev).
        (major & 0xFFF) << 8 | (minor & 0xFF) | (minor & !0xFF) << 12
    }

    fn getfattr(path: &Path, name: &str) -> Vec<u8> {
        // getfattr --only-values prints the raw value.
        let out = Command::new("getfattr")
            .args(["--only-values", "--absolute-names", "-n", name])
            .arg(path)
            .output()
            .expect("getfattr (install attr)");
        assert!(
            out.status.success(),
            "getfattr {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }
}

/// Effective uid without a libc dependency: /proc/self/status "Uid:"
/// line, second field.
fn effective_uid() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2))
        .and_then(|s| s.parse().ok())
        .unwrap_or(u32::MAX)
}
