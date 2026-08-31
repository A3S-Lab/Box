//! Writer gates: every image passes `e2fsck -fn`, the
//! reader round-trips it with zero verify() issues, the sink contract
//! holds exactly, and the output is byte-deterministic (and sensitive).

use mkext4::reader::Fs;
use mkext4::sink::{CheckingSink, VecSink};
use mkext4::spec;
use mkext4::{FsBuilder, InodeCount, Meta, Options, SparseSeg, ROOT};

mod common;
use common::{pattern_at, pattern_bytes, Pattern, EPOCH};

const UUID: [u8; 16] = [
    0xd0, 0xd0, 0xca, 0xca, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
];
const SEED: [u32; 4] = [0xefbe_adde, 0xad4e_adde, 0xadde_ad8e, 0x0000_efbe];

fn options(size: u64) -> Options {
    let mut o = Options::new(size, UUID, EPOCH);
    o.hash_seed = SEED;
    o.label = Some("mkext4".into());
    o
}

fn meta(mode: u16) -> Meta {
    Meta::new(mode, 0, 0, (EPOCH, 0))
}

/// Build a representative namespace and return the image bytes.
fn build_basic(size: u64, mtime_tweak: i64) -> Vec<u8> {
    let mut b = FsBuilder::new(options(size)).unwrap();
    b.set_meta(ROOT, meta(0o755)).unwrap();
    let usr = b.mkdir(ROOT, "usr", meta(0o755)).unwrap();
    let bin = b.mkdir(usr, "bin", meta(0o755)).unwrap();
    let mut m = meta(0o644);
    m.mtime = (EPOCH + mtime_tweak, 500_000_000);
    let cat = b.file(bin, "cat", m, 8_192).unwrap();
    b.hardlink(bin, "dog", cat).unwrap();
    b.file(usr, "empty", meta(0o600), 0).unwrap();
    let setuid = b.file(ROOT, "setuid", meta(0o4755), 100).unwrap();
    // A file whose final block is partial.
    let odd = b.file(usr, "odd", meta(0o644), 5_000).unwrap();
    // Many names to force a multi-block linear directory.
    let big = b.mkdir(ROOT, "big", meta(0o755)).unwrap();
    for i in 0..200 {
        b.file(big, format!("entry_{i:05}_padding"), meta(0o644), 0)
            .unwrap();
    }
    // Whiteout: declare then remove a populated subtree.
    let tmp = b.mkdir(ROOT, "tmp", meta(0o755)).unwrap();
    b.file(tmp, "junk", meta(0o644), 4096).unwrap();
    b.remove(ROOT, "tmp").unwrap();
    // Removed and re-added name.
    b.file(ROOT, "twice", meta(0o644), 0).unwrap();
    b.remove(ROOT, "twice").unwrap();
    let twice = b.file(ROOT, "twice", meta(0o600), 42).unwrap();

    let layout = b.seal().unwrap();
    let mut sink = CheckingSink::new(VecSink::default());
    let mut w = layout.writer(&mut sink).unwrap();
    w.fill(cat, &mut Pattern::new(8_192, 1)).unwrap();
    w.fill(odd, &mut Pattern::new(5_000, 2)).unwrap();
    w.fill(twice, &mut Pattern::new(42, 3)).unwrap();
    // Out-of-declaration-order fill: legal, only loses ascending offsets.
    w.fill(setuid, &mut Pattern::new(100, 4)).unwrap();
    let summary = w.finish().unwrap();
    assert_eq!(summary.image_len, size);
    let inner = sink.finish(size).expect("exactly-once coverage");
    inner.buf.clone()
}

#[test]
fn basic_image_fsck_clean_and_readable() {
    let size = 64 << 20; // 16384 blocks
    let image = build_basic(size, 0);
    assert_eq!(image.len() as u64, size);
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        "writer_basic",
    );

    let fs = Fs::open(&image[..]).expect("reader opens our image");
    let issues = fs.verify().expect("verify runs");
    assert!(
        issues.is_empty(),
        "verify issues: {:?}",
        &issues[..issues.len().min(5)]
    );

    // Content round-trips.
    let cat = fs.resolve("/usr/bin/cat").unwrap();
    assert_eq!(fs.read_file(cat).unwrap(), pattern_bytes(8_192, 1));
    let odd = fs.resolve("/usr/odd").unwrap();
    assert_eq!(fs.read_file(odd).unwrap(), pattern_bytes(5_000, 2));
    let twice = fs.resolve("/twice").unwrap();
    assert_eq!(fs.read_file(twice).unwrap(), pattern_bytes(42, 3));

    // Hardlink shares the inode, nlink 2.
    let dog = fs.resolve("/usr/bin/dog").unwrap();
    assert_eq!(dog, cat);
    assert_eq!(fs.inode(cat).unwrap().links_count, 2);

    // Metadata.
    let setuid = fs.inode(fs.resolve("/setuid").unwrap()).unwrap();
    assert_eq!(setuid.mode & 0o7777, 0o4755);
    let cat_inode = fs.inode(cat).unwrap();
    let (mt, ns) = spec::Inode::timestamp(cat_inode.mtime, cat_inode.mtime_extra);
    assert_eq!((mt, ns), (EPOCH, 500_000_000));

    // Whiteout semantics: /tmp is gone entirely; /twice was re-added.
    assert!(fs.lookup(spec::ROOT_INO, b"tmp").unwrap().is_none());
    let twice_inode = fs.inode(twice).unwrap();
    assert_eq!(twice_inode.mode & 0o7777, 0o600);
    assert_eq!(twice_inode.size, 42);

    // Multi-block linear dir reads completely.
    let big = fs.resolve("/big").unwrap();
    let n = fs
        .read_dir(big)
        .unwrap()
        .iter()
        .filter(|e| e.name != b"." && e.name != b"..")
        .count();
    assert_eq!(n, 200);

    // lost+found per convention.
    let lf = fs.resolve("/lost+found").unwrap();
    assert_eq!(lf, 11);
    let lf_inode = fs.inode(lf).unwrap();
    assert_eq!(lf_inode.size, 4 * 4096);
    assert_eq!(lf_inode.mode & 0o7777, 0o700);
}

#[test]
fn deterministic_and_sensitive() {
    let size = 32 << 20;
    let a = build_basic(size, 0);
    let b = build_basic(size, 0);
    assert_eq!(a, b, "identical inputs must produce identical bytes");
    let c = build_basic(size, 1); // one mtime changed by one second
    assert_ne!(a, c, "a changed mtime must change the output");
}

#[test]
fn raw_directory_names_and_symlink_targets_round_trip() {
    let size = 16 << 20;
    let raw_name = b"name-\xff";
    let raw_link = b"link-\xfe";
    let raw_target = b"../target-\xfd";
    let mut builder = FsBuilder::new(options(size)).unwrap();
    let file = builder.file(ROOT, raw_name, meta(0o640), 7).unwrap();
    builder
        .symlink(ROOT, raw_link, raw_target, meta(0o777))
        .unwrap();

    let layout = builder.seal().unwrap();
    let mut sink = VecSink::default();
    let mut writer = layout.writer(&mut sink).unwrap();
    writer.fill(file, &mut b"payload".as_slice()).unwrap();
    writer.finish().unwrap();

    let filesystem = Fs::open(&sink.buf[..]).unwrap();
    assert!(filesystem.verify().unwrap().is_empty());
    let file = filesystem
        .lookup(spec::ROOT_INO, raw_name)
        .unwrap()
        .expect("raw filename");
    assert_eq!(filesystem.read_file(file).unwrap(), b"payload");
    let link = filesystem
        .lookup(spec::ROOT_INO, raw_link)
        .unwrap()
        .expect("raw symlink name");
    assert_eq!(filesystem.symlink_target(link).unwrap(), raw_target);
}

#[test]
fn empty_namespace_image() {
    let size = 16 << 20;
    let b = FsBuilder::new(options(size)).unwrap();
    let layout = b.seal().unwrap();
    let mut sink = CheckingSink::new(VecSink::default());
    let w = layout.writer(&mut sink).unwrap();
    w.finish().unwrap();
    let image = sink.finish(size).unwrap().buf.clone();
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        "writer_empty",
    );
    let fs = Fs::open(&image[..]).unwrap();
    assert!(fs.verify().unwrap().is_empty());
    assert_eq!(fs.resolve("/lost+found").unwrap(), 11);
}

#[test]
fn odd_geometry_multiple_groups() {
    // 3.5 groups + a file crossing the group-1 backup: forces split
    // extents and exercises INODE_UNINIT groups and backup emission.
    let size = 114688 * 4096; // 3.5 groups
    let mut b = FsBuilder::new(options(size)).unwrap();
    let f = b.file(ROOT, "spanner", meta(0o644), 300 << 20).unwrap();
    let layout = b.seal().unwrap();
    let extents = layout.extents(f);
    assert!(extents.len() > 1, "file must split across reserved runs");
    let mut sink = CheckingSink::new(VecSink::default());
    let mut w = layout.writer(&mut sink).unwrap();
    w.fill(f, &mut Pattern::new(300 << 20, 9)).unwrap();
    w.finish().unwrap();
    let image = sink.finish(size).unwrap().buf.clone();
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        "writer_span",
    );
    let fs = Fs::open(&image[..]).unwrap();
    assert!(fs.verify().unwrap().is_empty());
    let ino = fs.resolve("/spanner").unwrap();
    let inode = fs.inode(ino).unwrap();
    let fx = fs.extents(ino, &inode).unwrap();
    assert!(fx.len() > 1);
    // Spot-check content at extent boundaries.
    let want = pattern_bytes(300 << 20, 9);
    let mut got = vec![0u8; 1 << 16];
    for probe in [0u64, (32768 * 4096) - 7, 150 << 20, (300 << 20) - 1] {
        let n = fs.read_file_at(&inode, &fx, probe, &mut got).unwrap();
        assert!(n > 0);
        assert_eq!(
            got[..n],
            want[probe as usize..probe as usize + n],
            "content mismatch at {probe}"
        );
    }
}

#[test]
fn extent_tree_depth_one() {
    // A 640 MiB file in a 1 GiB image: five 32768-block extents plus
    // backup-run splits force > 4 extents => a depth-1 extent tree.
    let size = 1 << 30;
    let len = 640u64 << 20;
    let mut b = FsBuilder::new(options(size)).unwrap();
    let f = b.file(ROOT, "huge", meta(0o644), len).unwrap();
    let layout = b.seal().unwrap();
    assert!(layout.extents(f).len() > 4, "must exceed the inline root");
    let mut sink = CheckingSink::new(VecSink::default());
    let mut w = layout.writer(&mut sink).unwrap();
    w.fill(f, &mut Pattern::new(len, 11)).unwrap();
    w.finish().unwrap();
    let image = sink.finish(size).unwrap().buf.clone();
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        "writer_tree",
    );
    let fs = Fs::open(&image[..]).unwrap();
    assert!(fs.verify().unwrap().is_empty());
    let ino = fs.resolve("/huge").unwrap();
    let inode = fs.inode(ino).unwrap();
    // Root must be an index node now.
    let root = inode.extent_root().unwrap();
    let h = spec::ExtentHeader::decode(root).unwrap();
    assert_eq!(h.depth, 1);
    // Content spot checks across the whole range.
    let fx = fs.extents(ino, &inode).unwrap();
    let mut got = vec![0u8; 4096];
    for probe in [0u64, len / 2, len - 4096] {
        fs.read_file_at(&inode, &fx, probe, &mut got).unwrap();
        let want = pattern_at(len, 11, probe, got.len());
        assert_eq!(got, want, "content at {probe}");
    }
}

#[test]
fn full_feature_namespace() {
    use mkext4::SpecialKind;
    let size = 128 << 20;
    let build = || {
        let mut b = FsBuilder::new(options(size)).unwrap();
        b.set_xattr(ROOT, "security.selinux", b"system_u:object_r:root_t:s0")
            .unwrap();
        // Symlinks at the fast/slow boundary.
        b.symlink(ROOT, "sym_1", "b", meta(0o777)).unwrap();
        b.symlink(ROOT, "sym_59", "a".repeat(59), meta(0o777))
            .unwrap();
        b.symlink(ROOT, "sym_60", "a".repeat(60), meta(0o777))
            .unwrap();
        b.symlink(ROOT, "sym_long", "t/".repeat(120), meta(0o777))
            .unwrap();
        // Specials.
        b.mknod(
            ROOT,
            "null",
            meta(0o666),
            SpecialKind::Char { major: 1, minor: 3 },
        )
        .unwrap();
        b.mknod(
            ROOT,
            "bigdev",
            meta(0o600),
            SpecialKind::Block {
                major: 254,
                minor: 70000,
            },
        )
        .unwrap();
        b.mknod(ROOT, "fifo", meta(0o644), SpecialKind::Fifo)
            .unwrap();
        b.mknod(ROOT, "sock", meta(0o644), SpecialKind::Socket)
            .unwrap();
        // xattrs: ibody-only, spill-to-block, on a dir.
        let f = b.file(ROOT, "attrs", meta(0o644), 10).unwrap();
        b.set_xattr(f, "user.small", b"v").unwrap();
        b.set_xattr(f, "user.big", &vec![0x42u8; 600]).unwrap();
        b.set_xattr(f, "security.capability", &[1, 0, 0, 2, 0, 0, 0, 0])
            .unwrap();
        let d = b.mkdir(ROOT, "xdir", meta(0o755)).unwrap();
        b.set_xattr(d, "user.on-a-dir", b"yes").unwrap();
        // htree: 2000 entries is far past the threshold.
        let big = b.mkdir(ROOT, "big", meta(0o755)).unwrap();
        for i in 0..2000 {
            b.file(big, format!("node_{i:06}_padpadpad"), meta(0o644), 0)
                .unwrap();
        }
        // Sparse: data / 1 GiB hole / data, partial tail.
        let sp = b
            .file_sparse(
                ROOT,
                "sparse",
                meta(0o644),
                &[
                    SparseSeg::Data(8192),
                    SparseSeg::Hole(1 << 30),
                    SparseSeg::Data(5000),
                ],
            )
            .unwrap();
        let layout = b.seal().unwrap();
        let mut sink = CheckingSink::new(VecSink::default());
        let mut w = layout.writer(&mut sink).unwrap();
        w.fill(f, &mut Pattern::new(10, 5)).unwrap();
        w.fill(sp, &mut Pattern::new(8192 + 5000, 6)).unwrap();
        w.finish().unwrap();
        sink.finish(size).unwrap().buf.clone()
    };
    let image = build();
    assert_eq!(build(), image, "full-feature build must be deterministic");
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        "writer_features",
    );

    let fs = Fs::open(&image[..]).unwrap();
    let issues = fs.verify().unwrap();
    assert!(
        issues.is_empty(),
        "verify: {:?}",
        &issues[..issues.len().min(5)]
    );

    // Symlinks.
    for (path, len, fast) in [
        ("/sym_1", 1usize, true),
        ("/sym_59", 59, true),
        ("/sym_60", 60, false),
        ("/sym_long", 240, false),
    ] {
        let ino = fs.resolve(path).unwrap();
        let inode = fs.inode(ino).unwrap();
        assert_eq!(
            inode.fast_symlink_target().is_some(),
            fast,
            "{path} fast/slow"
        );
        assert_eq!(fs.symlink_target(ino).unwrap().len(), len, "{path}");
    }

    // Specials.
    let null = fs.inode(fs.resolve("/null").unwrap()).unwrap();
    assert_eq!(null.dev_numbers(), Some((1, 3)));
    let bigdev = fs.inode(fs.resolve("/bigdev").unwrap()).unwrap();
    assert_eq!(bigdev.dev_numbers(), Some((254, 70000)));
    assert_eq!(
        fs.inode(fs.resolve("/fifo").unwrap()).unwrap().file_type(),
        spec::inode::FileType::Fifo
    );
    assert_eq!(
        fs.inode(fs.resolve("/sock").unwrap()).unwrap().file_type(),
        spec::inode::FileType::Socket
    );

    // xattrs.
    let root_attrs = fs.xattrs(spec::ROOT_INO).unwrap();
    assert_eq!(root_attrs[0].full_name().unwrap(), b"security.selinux");
    let attrs = fs.xattrs(fs.resolve("/attrs").unwrap()).unwrap();
    let by_name = |n: &[u8]| {
        attrs
            .iter()
            .find(|e| e.full_name().unwrap() == n)
            .unwrap_or_else(|| panic!("missing {}", String::from_utf8_lossy(n)))
            .clone()
    };
    assert_eq!(by_name(b"user.small").value, b"v");
    assert_eq!(by_name(b"user.big").value.len(), 600);
    assert_eq!(by_name(b"security.capability").value.len(), 8);
    let dir_attrs = fs.xattrs(fs.resolve("/xdir").unwrap()).unwrap();
    assert_eq!(dir_attrs[0].value, b"yes");

    // htree.
    let big = fs.resolve("/big").unwrap();
    let inode = fs.inode(big).unwrap();
    assert_ne!(inode.flags & spec::iflags::INDEX, 0, "big must be htree");
    let n = fs
        .read_dir(big)
        .unwrap()
        .iter()
        .filter(|e| e.name != b"." && e.name != b"..")
        .count();
    assert_eq!(n, 2000);
    for i in [0, 999, 1999] {
        fs.resolve(&format!("/big/node_{i:06}_padpadpad")).unwrap();
    }

    // Sparse: holes read as zeros; extents show the gap.
    let sp = fs.resolve("/sparse").unwrap();
    let inode = fs.inode(sp).unwrap();
    assert_eq!(inode.size, 8192 + (1 << 30) + 5000);
    let fx = fs.extents(sp, &inode).unwrap();
    assert!(fx.len() >= 2);
    let logical_blocks: u64 = fx.iter().map(|e| u64::from(e.len)).sum();
    assert_eq!(logical_blocks, 2 + 2, "only data blocks are mapped");
    let mut buf = vec![0u8; 4096];
    // Hole reads as zeros.
    fs.read_file_at(&inode, &fx, 8192 + 4096, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0));
    // Tail data round-trips.
    let want = pattern_at(8192 + 5000, 6, 8192, 100);
    fs.read_file_at(&inode, &fx, 1 << 30, &mut buf[..0])
        .unwrap();
    let mut got = vec![0u8; 100];
    fs.read_file_at(&inode, &fx, 8192 + (1 << 30), &mut got)
        .unwrap();
    assert_eq!(got, want, "post-hole data");
}

#[test]
fn htree_threshold_matches_oracle_rule() {
    // Entry bytes < 4072 => linear (even at 2 blocks); >= 4072 => htree.
    // 16-byte records ("e_NNN" names): 254 entries = 4064 B, 255 = 4080 B.
    for (count, expect_htree) in [(254u32, false), (255, true)] {
        let mut b = FsBuilder::new(options(16 << 20)).unwrap();
        let d = b.mkdir(ROOT, "d", meta(0o755)).unwrap();
        for i in 0..count {
            b.file(d, format!("e_{i:03}"), meta(0o644), 0).unwrap();
        }
        let layout = b.seal().unwrap();
        let mut sink = VecSink::default();
        layout.writer(&mut sink).unwrap().finish().unwrap();
        let image = sink.buf;
        common::assert_fsck_clean(
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
            &image,
            &format!("writer_thresh_{count}"),
        );
        let fs = Fs::open(&image[..]).unwrap();
        assert!(fs.verify().unwrap().is_empty(), "{count} entries");
        let inode = fs.inode(fs.resolve("/d").unwrap()).unwrap();
        assert_eq!(
            inode.flags & spec::iflags::INDEX != 0,
            expect_htree,
            "{count} entries"
        );
    }
}

#[test]
fn two_level_htree() {
    // ~140k entries exceeds one dx_root of leaves (507 * ~145/leaf).
    let size = 1 << 30;
    let mut opts = options(size);
    opts.inodes = InodeCount::Exact(150_000);
    let mut b = FsBuilder::new(opts).unwrap();
    let d = b.mkdir(ROOT, "huge", meta(0o755)).unwrap();
    for i in 0..140_000u32 {
        b.file(d, format!("node_{i:06}_padpadpad"), meta(0o644), 0)
            .unwrap();
    }
    let layout = b.seal().unwrap();
    let mut sink = VecSink::default();
    layout.writer(&mut sink).unwrap().finish().unwrap();
    let image = sink.buf;
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        "writer_htree2",
    );
    let fs = Fs::open(&image[..]).unwrap();
    // verify() re-hashes every one of the 140k names into its dx range.
    let issues = fs.verify().unwrap();
    assert!(issues.is_empty(), "{:?}", &issues[..issues.len().min(5)]);
    let huge = fs.resolve("/huge").unwrap();
    let n = fs
        .read_dir(huge)
        .unwrap()
        .iter()
        .filter(|e| e.name != b"." && e.name != b"..")
        .count();
    assert_eq!(n, 140_000);
    for i in [0u32, 77_777, 139_999] {
        fs.resolve(&format!("/huge/node_{i:06}_padpadpad")).unwrap();
    }
}

#[test]
fn flat_100k_entry_directory() {
    // The primary htree target shape: one directory, 100k entries.
    // Declaration must be far from quadratic (a regression makes this
    // test hang), removal/redeclare must stay exact, and the image must
    // pass every gate.
    let size = 1u64 << 30;
    let mut opts = options(size);
    opts.inodes = InodeCount::Exact(120_000);
    let mut b = FsBuilder::new(opts).unwrap();
    let d = b.mkdir(ROOT, "flat", meta(0o755)).unwrap();
    for i in 0..100_000u32 {
        b.file(d, format!("entry_{i:06}_pad"), meta(0o644), 0)
            .unwrap();
    }
    // Whiteout + redeclare a sample under the same names.
    for i in (0..100_000u32).step_by(1000) {
        b.remove(d, format!("entry_{i:06}_pad")).unwrap();
        b.file(d, format!("entry_{i:06}_pad"), meta(0o600), 0)
            .unwrap();
    }
    let layout = b.seal().unwrap();
    let mut sink = VecSink::default();
    layout.writer(&mut sink).unwrap().finish().unwrap();
    let image = sink.buf;
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        "writer_flat100k",
    );
    let fs = Fs::open(&image[..]).unwrap();
    assert!(fs.verify().unwrap().is_empty());
    let flat = fs.resolve("/flat").unwrap();
    let n = fs
        .read_dir(flat)
        .unwrap()
        .iter()
        .filter(|e| e.name != b"." && e.name != b"..")
        .count();
    assert_eq!(n, 100_000);
    // Redeclared entries carry the new mode.
    let ino = fs.resolve("/flat/entry_042000_pad").unwrap();
    assert_eq!(fs.inode(ino).unwrap().mode & 0o7777, 0o600);
}

#[test]
fn unfilled_file_errors_and_poisoning() {
    let size = 16 << 20;
    let mut b = FsBuilder::new(options(size)).unwrap();
    let f = b.file(ROOT, "f", meta(0o644), 4096).unwrap();
    let layout = b.seal().unwrap();
    // finish() without fill must error.
    let mut sink = VecSink::default();
    let w = layout.writer(&mut sink).unwrap();
    assert!(w.finish().is_err());
    // Short fill poisons.
    let mut sink = VecSink::default();
    let mut w = layout.writer(&mut sink).unwrap();
    assert!(w.fill(f, &mut Pattern::new(100, 0)).is_err()); // only 100 of 4096
    assert!(w.finish().is_err());
}
