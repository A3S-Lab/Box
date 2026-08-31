//! Hostile-image regression tests: corrupt superblock geometry must be
//! rejected with `Error::Corrupt`, never a panic or absurd allocation.
//! (Each corruption here previously panicked the reader — div-by-zero,
//! index out of bounds, or an unbounded Vec — before `Fs::open` grew its
//! geometry validation block.)

use mkext4::reader::Fs;
use mkext4::sink::VecSink;
use mkext4::{FsBuilder, Meta, Options, ROOT};

mod common;
use common::EPOCH;

/// A small valid image to corrupt.
fn valid_image() -> Vec<u8> {
    let mut b = FsBuilder::new(Options::new(16 << 20, [9; 16], EPOCH)).unwrap();
    b.file(ROOT, "f", Meta::new(0o644, 0, 0, (EPOCH, 0)), 0)
        .unwrap();
    let layout = b.seal().unwrap();
    let mut sink = VecSink::default();
    layout.writer(&mut sink).unwrap().finish().unwrap();
    sink.buf
}

/// Corrupt a superblock field and refresh the superblock checksum so the
/// geometry check (not the checksum check) is what rejects the image.
fn corrupt_sb(image: &mut [u8], offset: usize, bytes: &[u8]) {
    let sb = &mut image[1024..2048];
    sb[offset..offset + bytes.len()].copy_from_slice(bytes);
    let csum = mkext4::csum::superblock(sb);
    sb[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
}

fn assert_rejected(image: &[u8], what: &str) {
    match Fs::open(image) {
        Err(mkext4::Error::Corrupt { .. }) => {}
        other => panic!("{what}: expected Corrupt, got {other:?}"),
    }
}

#[test]
fn zeroed_blocks_per_group_rejected() {
    let mut img = valid_image();
    corrupt_sb(&mut img, 0x20, &0u32.to_le_bytes()); // s_blocks_per_group
    assert_rejected(&img, "blocks_per_group = 0");
}

#[test]
fn zeroed_inodes_per_group_rejected() {
    let mut img = valid_image();
    corrupt_sb(&mut img, 0x28, &0u32.to_le_bytes()); // s_inodes_per_group
    assert_rejected(&img, "inodes_per_group = 0");
}

#[test]
fn oversized_inodes_per_group_rejected() {
    let mut img = valid_image();
    corrupt_sb(&mut img, 0x28, &1_000_000u32.to_le_bytes());
    assert_rejected(&img, "inodes_per_group > bits in a bitmap block");
}

#[test]
fn bogus_inode_size_rejected() {
    let mut img = valid_image();
    corrupt_sb(&mut img, 0x58, &7u16.to_le_bytes()); // s_inode_size
    assert_rejected(&img, "inode_size = 7");
}

#[test]
fn huge_block_count_rejected() {
    // With the 64bit feature a forged blocks_count_hi would previously
    // size the GDT allocation at petabytes.
    let mut img = valid_image();
    let incompat = u32::from_le_bytes(img[1024 + 0x60..1024 + 0x64].try_into().unwrap()) | 0x80;
    corrupt_sb(&mut img, 0x60, &incompat.to_le_bytes());
    corrupt_sb(&mut img, 0x150, &u32::MAX.to_le_bytes()); // s_blocks_count_hi
    assert_rejected(&img, "blocks_count with forged high word");
}

#[test]
fn small_inodes_are_checksummable() {
    // csum::inode used to index past the end of 128-byte inode slots.
    let raw = [0u8; 128];
    let _ = mkext4::csum::inode(0x1234_5678, 2, 0, &raw); // must not panic
}

#[test]
fn deep_extent_tree_rejected() {
    // Forge depth 6 in the root extent header of the first file inode.
    let mut img = valid_image();
    let fs = Fs::open(&img[..]).unwrap();
    let ino = fs.resolve("/f").unwrap();
    // Locate the inode slot: group 0 itable + (ino-1)*256.
    let desc = fs.group_desc(0).unwrap();
    let off = (desc.inode_table * 4096 + u64::from(ino - 1) * 256) as usize;
    drop(fs);
    img[off + 0x28 + 6] = 6; // eh_depth
    let fs = Fs::open(&img[..]).unwrap();
    // Inode checksum now mismatches (Corrupt) or, if checks were skipped,
    // the extent walk must reject the depth — either way, no panic.
    let ino_res = fs.inode(ino);
    assert!(ino_res.is_err() || fs.extents(ino, &ino_res.unwrap()).is_err());
}
