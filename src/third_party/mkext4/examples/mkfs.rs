//! Build an ext4 image from a directory tree — the `mke2fs -d`
//! equivalent, and the subject of the benchmarks in `tools/bench.sh`.
//!
//! Usage: mkfs <source-dir> <output.img> <size-bytes>
//!
//! Deterministic by construction: fixed UUID/hash-seed/epoch, tree
//! walked in sorted order, hardlinks detected by (dev, ino).

use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use mkext4::sink::FileSink;
use mkext4::{FsBuilder, InodeCount, InodeHandle, Meta, Options, SpecialKind, ROOT};

fn meta_from(md: &std::fs::Metadata) -> Meta {
    let mut m = Meta::new(
        (md.mode() & 0o7777) as u16,
        md.uid(),
        md.gid(),
        (md.mtime(), md.mtime_nsec() as u32),
    );
    m.atime = Some((md.atime(), md.atime_nsec() as u32));
    m.ctime = Some((md.ctime(), md.ctime_nsec() as u32));
    m
}

fn declare(
    b: &mut FsBuilder,
    dir: InodeHandle,
    path: &Path,
    hardlinks: &mut HashMap<(u64, u64), InodeHandle>,
    fills: &mut Vec<(InodeHandle, std::path::PathBuf)>,
) -> mkext4::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .map(|e| e.unwrap())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().expect("non-UTF-8 name");
        let md = std::fs::symlink_metadata(entry.path()).unwrap();
        let meta = meta_from(&md);
        let ft = md.file_type();
        if ft.is_dir() {
            let child = b.mkdir(dir, name, meta)?;
            declare(b, child, &entry.path(), hardlinks, fills)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(entry.path()).unwrap();
            b.symlink(dir, name, target.to_str().expect("non-UTF-8 target"), meta)?;
        } else if ft.is_file() {
            let key = (md.dev(), md.ino());
            if md.nlink() > 1 {
                if let Some(&existing) = hardlinks.get(&key) {
                    b.hardlink(dir, name, existing)?;
                    continue;
                }
            }
            let f = b.file(dir, name, meta, md.len())?;
            if md.nlink() > 1 {
                hardlinks.insert(key, f);
            }
            if md.len() > 0 {
                fills.push((f, entry.path()));
            }
        } else if ft.is_char_device() || ft.is_block_device() {
            let (major, minor) = (rdev_major(md.rdev()), rdev_minor(md.rdev()));
            let kind = if ft.is_char_device() {
                SpecialKind::Char { major, minor }
            } else {
                SpecialKind::Block { major, minor }
            };
            b.mknod(dir, name, meta, kind)?;
        } else if ft.is_fifo() {
            b.mknod(dir, name, meta, SpecialKind::Fifo)?;
        } else if ft.is_socket() {
            b.mknod(dir, name, meta, SpecialKind::Socket)?;
        }
    }
    Ok(())
}

fn rdev_major(rdev: u64) -> u32 {
    ((rdev >> 8) & 0xFFF) as u32
}
fn rdev_minor(rdev: u64) -> u32 {
    ((rdev & 0xFF) | ((rdev >> 12) & !0xFFu64)) as u32
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, src, out, size] = &args[..] else {
        eprintln!("usage: mkfs <source-dir> <output.img> <size-bytes>");
        std::process::exit(2);
    };
    let size: u64 = size.parse().expect("size-bytes");

    // MKEXT4_INODES pins the inode count (benchmarks pass the same
    // value to mke2fs -N for a fair comparison).
    let inodes = std::env::var("MKEXT4_INODES")
        .ok()
        .map(|v| InodeCount::Exact(v.parse().expect("MKEXT4_INODES")))
        .unwrap_or(InodeCount::Auto);
    let mut opts = Options::new(size, *b"mkext4-benchmark", 1_704_067_200);
    opts.inodes = inodes;
    let mut b = FsBuilder::new(opts).expect("options");

    let mut hardlinks = HashMap::new();
    let mut fills = Vec::new();
    declare(&mut b, ROOT, Path::new(src), &mut hardlinks, &mut fills).expect("declare");

    let layout = b.seal().expect("seal");
    let mut sink = FileSink::create(Path::new(out), layout.image_len()).expect("create output");
    let mut w = layout.writer(&mut sink).expect("writer");
    for (handle, path) in &fills {
        let mut f = std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        w.fill(*handle, &mut f)
            .unwrap_or_else(|e| panic!("fill {}: {e}", path.display()));
    }
    let summary = w.finish().expect("finish");
    eprintln!("wrote {} ({} MiB data)", out, summary.data_bytes >> 20);
}
