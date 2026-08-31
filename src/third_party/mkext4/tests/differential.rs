//! Differential oracle: open real `mke2fs 1.47.x` images and verify the
//! reader agrees with the filesystem tree the image was built from, plus
//! facts injected via debugfs (devices, fifos, xattrs).
//!
//! Requires e2fsprogs (`mke2fs`, `debugfs`, `e2fsck`). Locate it via
//! `E2FSPROGS_SBIN`, the Homebrew keg path, or `PATH`; if absent, tests
//! skip with a note (CI always provides it).

use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use mkext4::reader::Fs;
use mkext4::spec::{self, inode::FileType};

mod common;
use common::e2fsprogs_sbin;

/// Generate the reference matrix once per test process. Returns the
/// output dir, or None when e2fsprogs is unavailable.
fn refs() -> Option<&'static Path> {
    static REFS: OnceLock<Option<PathBuf>> = OnceLock::new();
    REFS.get_or_init(|| {
        let sbin = match e2fsprogs_sbin() {
            Some(s) => s,
            None => {
                eprintln!("SKIP: e2fsprogs not found (set E2FSPROGS_SBIN)");
                return None;
            }
        };
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("refs");
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let status = std::process::Command::new("bash")
            .arg(manifest.join("tools/mkrefs.sh"))
            .arg(&out)
            .env("SKIP_BIG", "1")
            .env("E2SBIN", &sbin)
            .current_dir(&manifest)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .status()
            .expect("running tools/mkrefs.sh");
        assert!(status.success(), "tools/mkrefs.sh failed");
        Some(out)
    })
    .as_deref()
}

macro_rules! require_refs {
    () => {
        match refs() {
            Some(r) => r,
            None => return, // e2fsprogs unavailable: skip
        }
    };
}

fn open(img: &Path) -> Fs<std::fs::File> {
    Fs::open(std::fs::File::open(img).unwrap_or_else(|e| panic!("{}: {e}", img.display())))
        .expect("open image")
}

/// Recursively compare a source directory against the image directory
/// `ino`. Returns the number of entries compared.
fn compare_tree(fs: &Fs<std::fs::File>, src: &Path, ino: u32, allow_extra: &[&[u8]]) -> usize {
    let mut image: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
    for e in fs.read_dir(ino).expect("read_dir") {
        if e.name != b"." && e.name != b".." {
            image.insert(e.name, e.inode);
        }
    }

    let mut compared = 0;
    let mut source_names = Vec::new();
    for entry in std::fs::read_dir(src).expect("read_dir source") {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name_bytes = name.as_encoded_bytes().to_vec();
        source_names.push(name_bytes.clone());
        let child_ino = *image
            .get(&name_bytes)
            .unwrap_or_else(|| panic!("{}: missing from image", entry.path().display()));
        let meta = std::fs::symlink_metadata(entry.path()).unwrap();
        let inode = fs.inode(child_ino).expect("inode");

        // Type.
        let ft = meta.file_type();
        let want = if ft.is_dir() {
            FileType::Dir
        } else if ft.is_symlink() {
            FileType::Symlink
        } else if ft.is_socket() {
            FileType::Socket
        } else if ft.is_fifo() {
            FileType::Fifo
        } else {
            FileType::Regular
        };
        assert_eq!(inode.file_type(), want, "{}: type", entry.path().display());

        // Permissions, ownership, mtime seconds.
        assert_eq!(
            u32::from(inode.mode) & 0o7777,
            meta.permissions().mode() & 0o7777,
            "{}: mode",
            entry.path().display()
        );
        assert_eq!(inode.uid, meta.uid(), "{}: uid", entry.path().display());
        assert_eq!(inode.gid, meta.gid(), "{}: gid", entry.path().display());
        let (img_mtime, _) = spec::Inode::timestamp(inode.mtime, inode.mtime_extra);
        assert_eq!(img_mtime, meta.mtime(), "{}: mtime", entry.path().display());

        if ft.is_symlink() {
            let target = fs.symlink_target(child_ino).expect("symlink target");
            let want = std::fs::read_link(entry.path()).unwrap();
            assert_eq!(
                target,
                want.as_os_str().as_encoded_bytes(),
                "{}: symlink target",
                entry.path().display()
            );
        } else if ft.is_file() {
            assert_eq!(inode.size, meta.len(), "{}: size", entry.path().display());
            compare_content(fs, child_ino, &entry.path());
            // Hardlinks share the inode and count links.
            assert_eq!(
                u64::from(inode.links_count),
                meta.nlink(),
                "{}: nlink",
                entry.path().display()
            );
        } else if ft.is_dir() {
            compared += compare_tree(fs, &entry.path(), child_ino, &[]);
        }
        compared += 1;
    }

    // Reverse: everything in the image must exist in the source, modulo
    // the explicit allowlist (lost+found, debugfs-injected nodes).
    for name in image.keys() {
        let known = source_names.contains(name) || allow_extra.iter().any(|a| *a == &name[..]);
        assert!(
            known,
            "image has unexpected entry {:?} under {}",
            String::from_utf8_lossy(name),
            src.display()
        );
    }
    compared
}

fn compare_content(fs: &Fs<std::fs::File>, ino: u32, src: &Path) {
    use std::io::Read;
    let inode = fs.inode(ino).unwrap();
    let extents = fs.extents(ino, &inode).unwrap();
    let mut f = std::fs::File::open(src).unwrap();
    let mut want = vec![0u8; 1 << 20];
    let mut got = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    loop {
        let n = f.read(&mut want).unwrap();
        if n == 0 {
            break;
        }
        let m = fs
            .read_file_at(&inode, &extents, offset, &mut got[..n])
            .unwrap();
        assert_eq!(m, n, "{}: short read at {offset}", src.display());
        assert_eq!(
            got[..n],
            want[..n],
            "{}: content differs at {offset}",
            src.display()
        );
        offset += n as u64;
    }
    assert_eq!(offset, inode.size, "{}: size", src.display());
}

#[test]
fn differential_tree_ref512() {
    let refs = require_refs!();
    let fs = open(&refs.join("img/ref512.img"));
    let n = compare_tree(
        &fs,
        &refs.join("tree/t512"),
        spec::ROOT_INO,
        &[
            b"lost+found",
            b"dev_c_old",
            b"dev_b_old",
            b"dev_c_new",
            b"dev_b_new",
            b"fifo",
        ],
    );
    assert!(n > 300, "compared only {n} entries");
}

#[test]
fn debugfs_postop_facts_ref512() {
    let refs = require_refs!();
    let fs = open(&refs.join("img/ref512.img"));

    for (path, want) in [
        ("/dev_c_old", (5, 1)),
        ("/dev_b_old", (8, 16)),
        ("/dev_c_new", (254, 300)),
        ("/dev_b_new", (200, 65535)),
    ] {
        let ino = fs.resolve(path).unwrap();
        let inode = fs.inode(ino).unwrap();
        assert_eq!(inode.dev_numbers(), Some(want), "{path}");
    }
    let fifo = fs.inode(fs.resolve("/fifo").unwrap()).unwrap();
    assert_eq!(fifo.file_type(), FileType::Fifo);

    let x = fs.xattrs(fs.resolve("/xattr_ibody").unwrap()).unwrap();
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].full_name().unwrap(), b"user.small");
    assert_eq!(x[0].value, b"smallvalue");

    let x = fs.xattrs(fs.resolve("/xattr_block").unwrap()).unwrap();
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].full_name().unwrap(), b"user.big");
    assert_eq!(x[0].value.len(), 256);

    let x = fs.xattrs(fs.resolve("/xattr_mixed").unwrap()).unwrap();
    let names: Vec<_> = x.iter().map(|e| e.full_name().unwrap()).collect();
    for want in [
        &b"security.selinux"[..],
        b"user.alpha",
        b"user.zeta",
        b"user.beta",
    ] {
        assert!(names.iter().any(|n| n == want), "missing {want:?}");
    }
}

#[test]
fn verify_clean_on_all_images() {
    let refs = require_refs!();
    for img in ["ref16.img", "refodd.img", "ref512.img", "ref512dx.img"] {
        let fs = open(&refs.join("img").join(img));
        let issues = fs.verify().expect("verify runs");
        assert!(
            issues.is_empty(),
            "{img}: {} issues, first: {:?}",
            issues.len(),
            issues.first()
        );
    }
}

#[test]
fn htree_directory_reads_completely() {
    let refs = require_refs!();
    let fs = open(&refs.join("img/ref512dx.img"));
    let bigdir = fs.resolve("/bigdir").unwrap();
    let inode = fs.inode(bigdir).unwrap();
    assert_ne!(inode.flags & spec::iflags::INDEX, 0, "bigdir must be htree");

    let entries = fs.read_dir(bigdir).unwrap();
    let names: Vec<_> = entries
        .iter()
        .filter(|e| e.name != b"." && e.name != b"..")
        .collect();
    assert_eq!(names.len(), 300);
    // Spot-resolve through the htree structures.
    for i in [0, 137, 299] {
        let path = format!("/bigdir/entry_{i:05}_pad");
        fs.resolve(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    }
}

#[test]
fn geometry_facts() {
    let refs = require_refs!();
    let fs = open(&refs.join("img/refodd.img"));
    let sb = fs.superblock();
    assert_eq!(sb.blocks_count, 305000);
    assert_eq!(sb.group_count(), 10);
    assert_eq!(
        sb.inodes_per_group, 7632,
        "ipg rounds to fill itable blocks"
    );

    let fs16 = open(&refs.join("img/ref16.img"));
    assert_eq!(fs16.superblock().group_count(), 1);
    // Journal size tiers.
    assert_eq!(
        fs16.inode(spec::JOURNAL_INO).unwrap().size,
        1024 * 4096,
        "16MiB fs -> 1024-block journal"
    );
    assert_eq!(
        fs.inode(spec::JOURNAL_INO).unwrap().size,
        8192 * 4096,
        "305000-block fs -> 8192-block journal"
    );
}
