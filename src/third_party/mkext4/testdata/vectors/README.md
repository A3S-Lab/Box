# Extracted ext4 test vectors

Binary structures extracted from real `mke2fs 1.47.4` images, used as golden
fixtures for mkext4's on-disk encoders and checksum primitives. Every
checksum inside these blobs has been recomputed and verified by
`tools/check_vectors.py check`; the Rust unit tests must reproduce them too.

Provenance: images built by `tools/mkrefs.sh` (fixed UUID
`d0d0caca-0000-4000-8000-000000000001`, fixed htree hash seed
`deadbeef-dead-4ead-8ead-deadbeef0000`, feature set exactly matching the
crate: `has_journal,ext_attr,dir_index,filetype,extent,flex_bg,sparse_super,
large_file,huge_file,extra_isize,metadata_csum`, 4096-byte blocks, 256-byte
inodes). All blobs come from the 512 MiB image `ref512.img`; the htree blobs
(`dx_*`, `inode_bigdir_dx`) come from `ref512dx.img`, the variant re-indexed
with `e2fsck -fD` (mke2fs itself does not build htree). `manifest.json`
records each blob's source image, byte offset, and owning inode.

Note: blob *contents* are stable given the toolchain version but not
bit-reproducible across regenerations (mke2fs stamps wall-clock times), so
regenerating rewrites history in this directory; the committed copies are the
canonical fixtures. To regenerate: `tools/mkrefs.sh build/refs && python3
tools/check_vectors.py dump build/refs/img/ref512.img
build/refs/img/ref512dx.img testdata/vectors`.

`dx_hash.json` holds half_md4(signed, seed) directory-hash vectors confirmed
against `debugfs -R "dx_hash -h half_md4 -s <seed> <name>"`.
