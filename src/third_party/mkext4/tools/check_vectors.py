#!/usr/bin/env python3
"""Verify mkext4's documented ext4 checksum/hash algorithms against real
mke2fs images, and extract test-vector blobs for the Rust unit tests.

This is a deliberately independent reimplementation of every on-disk
algorithm DESIGN.md specifies (crc32c seeding/coverage, half_md4 dirhash,
xattr hashes, structure offsets). Each check prints `OK <what>` or
`FAIL <what>`; exit status is non-zero if anything fails.

Usage:
  tools/check_vectors.py check <image> [<imagedx>]   # run all checks
  tools/check_vectors.py dump  <image> <imagedx> <outdir>   # write vectors

The `check` mode expects images produced by tools/mkrefs.sh (it looks up
fixture paths like /sub, /xattr_block by walking the fs itself).
"""

import json
import struct
import sys

BS = 4096

# --- crc32c: Castagnoli, reflected, poly 0x82F63B78, NO pre/post inversion --
# (matches ext2fs_crc32c_le / kernel crc32c_le: the caller supplies the seed;
# ext4 never applies a final xor)

_TBL = []
for _i in range(256):
    _c = _i
    for _ in range(8):
        _c = (_c >> 1) ^ 0x82F63B78 if _c & 1 else _c >> 1
    _TBL.append(_c)


def crc32c(seed: int, data: bytes) -> int:
    c = seed & 0xFFFFFFFF
    for b in data:
        c = _TBL[(c ^ b) & 0xFF] ^ (c >> 8)
    return c


# --- half_md4 directory hash (kernel fs/ext4/hash.c + lib/halfmd4.c) --------

def _rol32(x, s):
    x &= 0xFFFFFFFF
    return ((x << s) | (x >> (32 - s))) & 0xFFFFFFFF


def _half_md4_transform(buf, inp):
    """lib/halfmd4.c: 3 rounds, target order a,d,c,b within each round."""
    def F(x, y, z): return z ^ (x & (y ^ z))
    def G(x, y, z): return ((x & y) + ((x ^ y) & z)) & 0xFFFFFFFF
    def H(x, y, z): return x ^ y ^ z
    rounds = (
        (F, 0x00000000, ((0, 3), (1, 7), (2, 11), (3, 19), (4, 3), (5, 7), (6, 11), (7, 19))),
        (G, 0x5A827999, ((1, 3), (3, 5), (5, 9), (7, 13), (0, 3), (2, 5), (4, 9), (6, 13))),
        (H, 0x6ED9EBA1, ((3, 3), (7, 9), (2, 11), (6, 15), (1, 3), (5, 9), (0, 11), (4, 15))),
    )
    v = list(buf)
    for f, k, sched in rounds:
        for i, (x, s) in enumerate(sched):
            t = (0, 3, 2, 1)[i % 4]
            args = (v[(t + 1) % 4], v[(t + 2) % 4], v[(t + 3) % 4])
            v[t] = _rol32((v[t] + f(*args) + inp[x] + k) & 0xFFFFFFFF, s)
    for i in range(4):
        buf[i] = (buf[i] + v[i]) & 0xFFFFFFFF


def _str2hashbuf_signed(msg: bytes, num: int):
    ln = len(msg)
    pad = (ln | (ln << 8)) & 0xFFFF
    pad |= pad << 16
    val = pad
    out = []
    if ln > num * 4:
        ln = num * 4
    for i in range(ln):
        sb = msg[i] - 256 if msg[i] >= 128 else msg[i]  # signed char
        val = (sb + (val << 8)) & 0xFFFFFFFF
        if i % 4 == 3:
            out.append(val)
            val = pad
            num -= 1
    num -= 1
    if num >= 0:
        out.append(val)
        num -= 1
    while num >= 0:
        out.append(pad)
        num -= 1
    return out


def dx_hash_half_md4_signed(seed, name: bytes):
    """Returns (hash, minor_hash) as debugfs dx_hash reports them."""
    buf = list(seed) if any(seed) else [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476]
    p = name
    while True:
        chunk, p = p[:32], p[32:]
        _half_md4_transform(buf, _str2hashbuf_signed(chunk, 8))
        if len(p) == 0:
            break
    return buf[1] & ~1 & 0xFFFFFFFF, buf[2]


# --- xattr hashes (kernel fs/ext4/xattr.c legacy hash) -----------------------

def xattr_entry_hash(name: bytes, value: bytes) -> int:
    h = 0
    for ch in name:
        h = ((h << 5) ^ (h >> 27) ^ ch) & 0xFFFFFFFF
    if value:
        v = value + b"\0" * (-len(value) % 4)
        for i in range(0, len(v), 4):
            (w,) = struct.unpack_from("<I", v, i)
            h = ((h << 16) ^ (h >> 16) ^ w) & 0xFFFFFFFF
    return h


def xattr_block_hash(entry_hashes) -> int:
    h = 0
    for eh in entry_hashes:
        h = ((h << 16) ^ (h >> 16) ^ eh) & 0xFFFFFFFF
    return h


# --- minimal ext4 reader -----------------------------------------------------

def u16(b, o): return struct.unpack_from("<H", b, o)[0]
def u32(b, o): return struct.unpack_from("<I", b, o)[0]
def u64(b, o): return struct.unpack_from("<Q", b, o)[0]


class Fs:
    def __init__(self, path):
        self.f = open(path, "rb")
        self.sb = self.blk(0)[1024:2048]
        sb = self.sb
        assert u16(sb, 0x38) == 0xEF53, "bad magic"
        self.inodes_count = u32(sb, 0x00)
        self.blocks_count = u32(sb, 0x04)
        self.ipg = u32(sb, 0x28)
        self.bpg = u32(sb, 0x20)
        self.inode_size = u16(sb, 0x58)
        self.uuid = sb[0x68:0x78]
        self.hash_seed = struct.unpack_from("<4I", sb, 0xEC)
        self.desc_size_field = u16(sb, 0xFE)
        self.desc_size = self.desc_size_field or 32
        self.ngroups = (self.blocks_count + self.bpg - 1) // self.bpg
        self.seed = crc32c(0xFFFFFFFF, self.uuid)

    def blk(self, n, count=1):
        self.f.seek(n * BS)
        return self.f.read(BS * count)

    def desc(self, g):
        gdt = self.blk(1, (self.ngroups * self.desc_size + BS - 1) // BS)
        return gdt[g * self.desc_size:(g + 1) * self.desc_size]

    def inode_raw(self, ino):
        g, idx = divmod(ino - 1, self.ipg)
        itable = u32(self.desc(g), 0x08)
        off = itable * BS + idx * self.inode_size
        self.f.seek(off)
        return self.f.read(self.inode_size), off

    def inode_extents(self, raw):
        """[(logical, physical, len)] — inline root or depth-1 tree."""
        return self._parse_extent_node(raw[0x28:0x64], top=True)

    def _parse_extent_node(self, node, top=False):
        assert u16(node, 0) == 0xF30A, "extent magic"
        entries, depth = u16(node, 2), u16(node, 6)
        out = []
        for i in range(entries):
            e = node[12 + 12 * i:24 + 12 * i]
            if depth == 0:
                # ee_len <= 32768: initialized; > 32768: unwritten, len-32768
                ln = u16(e, 4)
                ln = ln - 32768 if ln > 32768 else ln
                phys = u16(e, 6) << 32 | u32(e, 8)
                out.append((u32(e, 0), phys, ln))
            else:
                child = u32(e, 4) | u16(e, 8) << 32
                out.extend(self._parse_extent_node(self.blk(child)[:BS]))
        return out

    def file_block(self, raw, logical):
        for lo, phys, ln in self.inode_extents(raw):
            if lo <= logical < lo + ln:
                return phys + (logical - lo)
        return None

    def dirents(self, block):
        """Yield (inode, file_type, name) from one linear dirent block."""
        off = 0
        while off < BS:
            ino, rl, nl, ft = u32(block, off), u16(block, off + 4), block[off + 6], block[off + 7]
            if rl == 0:
                break
            if ino != 0:
                yield ino, ft, block[off + 8:off + 8 + nl]
            off += rl

    def lookup(self, parent_ino, name: bytes):
        raw, _ = self.inode_raw(parent_ino)
        for lo, phys, ln in self.inode_extents(raw):
            for i in range(ln):
                for ino, ft, nm in self.dirents(self.blk(phys + i)):
                    if nm == name:
                        return ino
        return None

    def resolve(self, path):
        ino = 2
        for part in path.strip("/").split("/"):
            ino = self.lookup(ino, part.encode())
            if ino is None:
                raise KeyError(path)
        return ino


# --- checks ------------------------------------------------------------------

FAILURES = []


def check(what, ok, detail=""):
    print(("OK   " if ok else "FAIL ") + what + (f"  [{detail}]" if detail and not ok else ""))
    if not ok:
        FAILURES.append(what)


def ino_seed(fs, ino, raw):
    gen = u32(raw, 0x64)
    s = crc32c(fs.seed, struct.pack("<I", ino))
    return crc32c(s, struct.pack("<I", gen))


def check_superblock(fs):
    sb = fs.sb
    stored = u32(sb, 0x3FC)
    check("superblock csum = crc32c(~0, sb[0:0x3FC])", crc32c(0xFFFFFFFF, sb[:0x3FC]) == stored)
    check("s_desc_size == 0 when !64bit", fs.desc_size_field == 0, f"got {fs.desc_size_field}")
    check("s_checksum_type == 1", sb[0x175] == 1)
    check("s_first_data_block == 0", u32(sb, 0x14) == 0)
    # backup superblock at group 1
    if fs.ngroups > 1:
        bsb = fs.blk(fs.bpg)[:1024]
        check("backup sb csum valid", crc32c(0xFFFFFFFF, bsb[:0x3FC]) == u32(bsb, 0x3FC))
        check("backup sb s_block_group_nr == 1", u16(bsb, 0x5A) == 1)
        # mke2fs quirks: backups hold pre-finalization free counts, state=0,
        # and a stale s_kbytes_written; debugfs post-ops also only touch the
        # primary. Normalize those plus group_nr/csum, then require equality.
        prim = bytearray(sb)
        back = bytearray(bsb)
        for buf in (prim, back):
            buf[0x0C:0x14] = bytes(8)          # s_free_{blocks,inodes}_count
            buf[0x3A:0x3C] = b"\0\0"           # s_state
            buf[0x5A:0x5C] = b"\0\0"           # s_block_group_nr
            buf[0x178:0x180] = bytes(8)        # s_kbytes_written
            buf[0x3FC:0x400] = b"\0\0\0\0"     # s_checksum
        diffs = [hex(i) for i in range(1024) if prim[i] != back[i]]
        check("backup sb == primary modulo group_nr/csum/state/free-counts/kbytes",
              not diffs, f"also differs at {diffs[:8]}")


def check_group_descs(fs):
    ok_desc = ok_bb = ok_ib = True
    uninit_zero = True
    detail = ""
    for g in range(fs.ngroups):
        d = fs.desc(g)
        # descriptor checksum
        c = crc32c(fs.seed, struct.pack("<I", g))
        c = crc32c(c, d[:0x1E])
        c = crc32c(c, b"\0\0")
        if (c & 0xFFFF) != u16(d, 0x1E):
            ok_desc = False; detail = f"group {g}"
        flags = u16(d, 0x12)
        # block bitmap csum: full block (clusters_per_group/8 bytes);
        # BLOCK_UNINIT groups store 0 and an all-zero bitmap block
        bb = fs.blk(u32(d, 0x00))
        if flags & 0x2:
            if u16(d, 0x18) != 0 or any(bb):
                ok_bb = False; detail = f"group {g} BLOCK_UNINIT not zero"
        elif (crc32c(fs.seed, bb[:fs.bpg // 8]) & 0xFFFF) != u16(d, 0x18):
            ok_bb = False; detail = f"group {g} block bitmap"
        # inode bitmap csum: only (ipg+7)/8 bytes are covered
        ib = fs.blk(u32(d, 0x04))
        want = u16(d, 0x1A)
        got = crc32c(fs.seed, ib[:(fs.ipg + 7) // 8]) & 0xFFFF
        if flags & 0x1:  # INODE_UNINIT: mke2fs stores 0
            if want not in (0, got):
                uninit_zero = False; detail = f"group {g} uninit ib csum {want:#x}"
        elif got != want:
            ok_ib = False; detail = f"group {g} inode bitmap {got:#x} != {want:#x}"
    check("group desc csum = crc32c(seed, group_le32 || desc[:0x1E] || 0u16) & 0xffff", ok_desc, detail)
    check("block bitmap csum covers bpg/8 bytes", ok_bb, detail)
    check("inode bitmap csum covers (ipg+7)/8 bytes", ok_ib, detail)
    check("INODE_UNINIT groups store ib csum 0 (or valid)", uninit_zero, detail)
    n_bu = sum(1 for g in range(fs.ngroups) if u16(fs.desc(g), 0x12) & 0x2)
    print(f"note {n_bu}/{fs.ngroups} groups have BLOCK_UNINIT (fully-free groups)")
    check("all groups ITABLE_ZEROED", all(u16(fs.desc(g), 0x12) & 0x4 for g in range(fs.ngroups)))


def check_inode_csums(fs, inos):
    ok = True
    detail = ""
    for ino in inos:
        raw, _ = fs.inode_raw(ino)
        body = bytearray(raw)
        body[0x7C:0x7E] = b"\0\0"
        has_hi = u16(raw, 0x80) >= 4  # extra_isize covers i_checksum_hi
        if has_hi:
            body[0x82:0x84] = b"\0\0"
        c = crc32c(ino_seed(fs, ino, raw), bytes(body))
        want = u16(raw, 0x7C) | (u16(raw, 0x82) << 16 if has_hi else 0)
        got = c if has_hi else c & 0xFFFF
        if got != want:
            ok = False; detail = f"inode {ino}: {got:#x} != {want:#x}"
    check(f"inode csum (seed+ino+gen, csum fields zeroed) for {inos}", ok, detail)


def check_reserved_inodes(fs):
    raw1, _ = fs.inode_raw(1)
    check("inode 1: links=0, mode=0, nonzero times",
          u16(raw1, 0x1A) == 0 and u16(raw1, 0) == 0 and u32(raw1, 0x0C) != 0)
    # unused reserved inodes are all-zero EXCEPT a valid l_i_checksum_lo
    # (extra_isize=0 there, so i_checksum_hi does not apply)
    ok = True
    for i in (3, 4, 5, 6, 7, 9, 10):
        raw, _ = fs.inode_raw(i)
        body = bytearray(raw)
        body[0x7C:0x7E] = b"\0\0"
        if any(body) or (crc32c(ino_seed(fs, i, raw), bytes(body)) & 0xFFFF) != u16(raw, 0x7C):
            ok = False
    check("reserved inodes 3-7,9,10: zero body + valid csum_lo", ok)
    check_inode_csums(fs, [1, 2, 8, 11])


def check_root_dir_block(fs):
    raw, _ = fs.inode_raw(2)
    phys = fs.file_block(raw, 0)
    blk = fs.blk(phys)
    # tail: fake dirent {inode 0, rec_len 12, name_len 0, file_type 0xDE}
    t = blk[BS - 12:]
    check("dirent tail shape (0, 12, 0, 0xde)",
          u32(t, 0) == 0 and u16(t, 4) == 12 and t[6] == 0 and t[7] == 0xDE)
    c = crc32c(ino_seed(fs, 2, raw), blk[:BS - 12])
    check("dirent block csum covers block[0:4084]", c == u32(t, 8), f"{c:#x} != {u32(t,8):#x}")


def check_empty_dir_blocks(fs):
    """lost+found blocks 1..3 are 'empty' dirent blocks."""
    ino = fs.resolve("/lost+found")
    raw, _ = fs.inode_raw(ino)
    phys = fs.file_block(raw, 1)
    blk = fs.blk(phys)
    check("empty dir block: one unused dirent rec_len 4084 + tail",
          u32(blk, 0) == 0 and u16(blk, 4) == BS - 12,
          f"ino={u32(blk,0)} rl={u16(blk,4)}")
    c = crc32c(ino_seed(fs, ino, raw), blk[:BS - 12])
    check("empty dir block csum", c == u32(blk, BS - 4))


def check_extent_tree(fs, path):
    ino = fs.resolve(path)
    raw, _ = fs.inode_raw(ino)
    root = raw[0x28:0x64]
    depth = u16(root, 6)
    check(f"{path}: root eh_max == 4", u16(root, 4) == 4)
    if depth == 0:
        print(f"note {path}: depth 0, no interior blocks to check")
        return
    seed = ino_seed(fs, ino, raw)
    child = u32(root, 16) | u16(root, 20) << 32
    nb = fs.blk(child)
    check(f"{path}: leaf eh_magic/depth", u16(nb, 0) == 0xF30A and u16(nb, 6) == 0)
    check(f"{path}: leaf eh_max == 340 ((4096-12-4)/12)", u16(nb, 4) == 340, f"got {u16(nb,4)}")
    c = crc32c(seed, nb[:12 + 340 * 12])
    stored = u32(nb, 12 + 340 * 12)
    check(f"{path}: extent block csum covers header+eh_max entries (4092 B)",
          c == stored, f"{c:#x} != {stored:#x}")


def check_empty_file(fs, path):
    ino = fs.resolve(path)
    raw, _ = fs.inode_raw(ino)
    root = raw[0x28:0x64]
    check(f"{path}: empty file has extent header (magic, 0 entries, max 4, depth 0)",
          u32(raw, 0x20) & 0x80000 != 0 and u16(root, 0) == 0xF30A
          and u16(root, 2) == 0 and u16(root, 4) == 4 and u16(root, 6) == 0)


def check_symlinks(fs):
    ino = fs.resolve("/sym_59")
    raw, _ = fs.inode_raw(ino)
    check("sym_59: fast (target in i_block, no EXTENTS_FL, size 59)",
          u32(raw, 0x20) & 0x80000 == 0 and raw[0x28:0x28 + 59] == b"a" * 59
          and u32(raw, 0x04) == 59 and raw[0x28 + 59] == 0)
    ino = fs.resolve("/sym_60")
    raw, _ = fs.inode_raw(ino)
    ok = u32(raw, 0x20) & 0x80000 != 0 and u32(raw, 0x04) == 60
    phys = fs.file_block(raw, 0)
    blk = fs.blk(phys)
    check("sym_60: slow (extent block, target + zero padding)",
          ok and blk[:60] == b"a" * 60 and not any(blk[60:]))


def check_devices(fs):
    ino = fs.resolve("/dev_c_old")
    raw, _ = fs.inode_raw(ino)
    check("dev_c_old (5,1): old encoding i_block[0]=(maj<<8)|min",
          u32(raw, 0x28) == (5 << 8) | 1 and u32(raw, 0x2C) == 0)
    ino = fs.resolve("/dev_c_new")
    raw, _ = fs.inode_raw(ino)
    want = (300 & 0xFF) | (254 << 8) | ((300 & ~0xFF) << 12)
    check("dev_c_new (254,300): new encoding in i_block[1]",
          u32(raw, 0x28) == 0 and u32(raw, 0x2C) == want,
          f"{u32(raw,0x2C):#x} != {want:#x}")


def check_xattr_block(fs):
    ino = fs.resolve("/xattr_block")
    raw, _ = fs.inode_raw(ino)
    acl = u32(raw, 0x68)
    check("xattr_block: i_file_acl set, i_blocks includes it",
          acl != 0 and u32(raw, 0x1C) == 8)
    blk = fs.blk(acl)
    check("xattr block header magic/refcount/blocks", u32(blk, 0) == 0xEA020000
          and u32(blk, 4) == 1 and u32(blk, 8) == 1)
    body = bytearray(blk)
    body[0x10:0x14] = b"\0\0\0\0"
    c = crc32c(crc32c(fs.seed, struct.pack("<Q", acl)), bytes(body))
    check("xattr block csum = crc32c(seed, le64(blocknr) || block[h_checksum=0])",
          c == u32(blk, 0x10), f"{c:#x} != {u32(blk,0x10):#x}")
    # entry hash + block hash (entry at 0x20: name_len,index,value_offs...)
    nl, idx = blk[0x20], blk[0x21]
    voff, vsize = u16(blk, 0x22), u32(blk, 0x28)
    name = blk[0x30:0x30 + nl]
    value = blk[voff:voff + vsize]
    eh = xattr_entry_hash(name, value)
    check("xattr entry e_hash (legacy 5/27 name, 16/16 value rolls)",
          eh == u32(blk, 0x2C), f"{eh:#x} != {u32(blk,0x2C):#x}")
    stored_hh = u32(blk, 0x0C)
    if stored_hh == 0:
        print("note xattr h_hash stored as 0 (debugfs ea_set quirk; fsck accepts)")
    else:
        check("xattr block h_hash (fold of e_hashes)", xattr_block_hash([eh]) == stored_hh)


def check_xattr_ibody(fs):
    ino = fs.resolve("/xattr_mixed")
    raw, _ = fs.inode_raw(ino)
    extra = u16(raw, 0x80)
    base = 0x80 + extra
    check("ibody xattr magic 0xEA020000 after extra_isize area", u32(raw, base) == 0xEA020000)
    # first entry follows the 4-byte magic
    e = base + 4
    names = []
    while u32(raw, e) != 0:
        nl, idx = raw[e], raw[e + 1]
        names.append((idx, raw[e + 16:e + 16 + nl]))
        check(f"ibody xattr entry {raw[e+16:e+16+nl]!r}: e_hash == 0", u32(raw, e + 12) == 0)
        e += 16 + nl + (-nl % 4)
    print(f"note ibody xattr entry order: {names}")


def check_htree(fs, dirpath, extract_file=None):
    ino = fs.resolve(dirpath)
    raw, _ = fs.inode_raw(ino)
    check(f"{dirpath}: INDEX_FL set", u32(raw, 0x20) & 0x1000 != 0)
    b0 = fs.blk(fs.file_block(raw, 0))
    check(f"{dirpath}: dx_root info (hash_version=1 half_md4, info_length=8, unused=0)",
          b0[0x1C] == 1 and b0[0x1D] == 8 and b0[0x1F] == 0)
    levels = b0[0x1E]
    limit, count = u16(b0, 0x20), u16(b0, 0x22)
    check(f"{dirpath}: dx_root limit == (4096-32)/8 - 1 == 507", limit == 507, f"got {limit}")
    seed = ino_seed(fs, ino, raw)

    def dx_csum(blkbuf, count_offset, count, limit):
        # kernel ext4_dx_csum: buf[0:count_offset+count*8], then dt_reserved,
        # then 4 zero bytes standing in for dt_checksum
        toff = count_offset + limit * 8
        c = crc32c(seed, blkbuf[:count_offset + count * 8])
        c = crc32c(c, blkbuf[toff:toff + 4])
        return crc32c(c, b"\0\0\0\0"), u32(blkbuf, toff + 4)

    c, stored = dx_csum(b0, 0x20, count, limit)
    check(f"{dirpath}: dx_root csum (count-covered entries + zeroed tail)",
          c == stored, f"{c:#x} != {stored:#x} (limit {limit} count {count} levels {levels})")

    entries = [(u32(b0, 0x28 + 8 * i), u32(b0, 0x2C + 8 * i)) for i in range(count - 1)]
    entries.insert(0, (0, u32(b0, 0x24)))
    if levels:
        nb = fs.blk(fs.file_block(raw, entries[0][1]))
        nl_limit, nl_count = u16(nb, 0x08), u16(nb, 0x0A)
        check(f"{dirpath}: dx_node fake dirent (ino 0, rec_len 4096)",
              u32(nb, 0) == 0 and u16(nb, 4) == BS)
        check(f"{dirpath}: dx_node limit == 510", nl_limit == 510, f"got {nl_limit}")
        c, stored = dx_csum(nb, 0x8, nl_count, nl_limit)
        check(f"{dirpath}: dx_node csum", c == stored, f"{c:#x} != {stored:#x}")
        leaf_entries = [(u32(nb, 0x10 + 8 * i), u32(nb, 0x14 + 8 * i)) for i in range(nl_count - 1)]
        leaf_entries.insert(0, (0, u32(nb, 0x0C)))
        entries = leaf_entries  # verify hashes against level-1 ranges below
    # hash every name in each leaf and verify it falls in the dx range
    ok = True
    detail = ""
    checked = 0
    for i, (lo, blkno) in enumerate(entries[:6]):
        hi = entries[i + 1][0] if i + 1 < len(entries) else 0x100000000
        leaf = fs.blk(fs.file_block(raw, blkno))
        for e_ino, ft, name in fs.dirents(leaf):
            if name in (b".", b".."):
                continue
            h, minor = dx_hash_half_md4_signed(fs.hash_seed, name)
            if not (lo & ~1) <= h < hi:
                ok = False; detail = f"{name!r}: {h:#x} not in [{lo:#x},{hi:#x})"
            checked += 1
    check(f"{dirpath}: half_md4(signed, seed) of {checked} leaf names within dx ranges", ok, detail)


def check_journal(fs):
    raw, _ = fs.inode_raw(8)
    check("journal inode: mode 0600, links 1, EXTENTS_FL",
          u16(raw, 0) == 0o100600 and u16(raw, 0x1A) == 1 and u32(raw, 0x20) & 0x80000 != 0)
    jsize = u32(raw, 0x04) | (u32(raw, 0x6C) << 32)
    jblocks = jsize // BS
    check("journal i_blocks == journal blocks * 8", u32(raw, 0x1C) == jblocks * 8)
    jsb_phys = fs.file_block(raw, 0)
    jsb = fs.blk(jsb_phys)[:1024]
    B = struct.Struct(">I")
    magic, btype = B.unpack_from(jsb, 0)[0], B.unpack_from(jsb, 4)[0]
    check("jsb magic 0xC03B3998, blocktype 4 (v2)", magic == 0xC03B3998 and btype == 4)
    check("jsb blocksize/maxlen/first/sequence/start",
          B.unpack_from(jsb, 12)[0] == BS and B.unpack_from(jsb, 16)[0] == jblocks
          and B.unpack_from(jsb, 20)[0] == 1 and B.unpack_from(jsb, 24)[0] == 1
          and B.unpack_from(jsb, 28)[0] == 0)
    feat_compat, feat_incompat, feat_ro = (B.unpack_from(jsb, o)[0] for o in (36, 40, 44))
    print(f"note jsb features: compat={feat_compat:#x} incompat={feat_incompat:#x} ro={feat_ro:#x}")
    print(f"note jsb s_uuid == fs uuid: {jsb[48:64] == fs.uuid}")
    print(f"note jsb s_nr_users={B.unpack_from(jsb,64)[0]} checksum_type={jsb[0x50]}")
    csum_stored = B.unpack_from(jsb, 0xFC)[0]
    body = bytearray(jsb)
    body[0xFC:0x100] = b"\0\0\0\0"
    jseed = crc32c(0xFFFFFFFF, jsb[48:64])
    variants = {
        "crc32c(~0, jsb[csum=0])": crc32c(0xFFFFFFFF, bytes(body)),
        "crc32c(juuid_seed, jsb[csum=0])": crc32c(jseed, bytes(body)),
    }
    if csum_stored == 0 and feat_incompat & 0x10 == 0:
        print("note jsb: no CSUM_V3, checksum field 0 (empty journal is un-checksummed)")
    else:
        match = [k for k, v in variants.items() if v == csum_stored]
        check("jsb csum formula identified", bool(match), f"stored {csum_stored:#x} vs {variants}")
        if match:
            print(f"note jsb csum = {match[0]}")
    # superblock journal backup
    sjnl = fs.sb[0x10C:0x10C + 17 * 4]
    check("s_jnl_backup_type == 1", fs.sb[0xFD] == 1)
    check("s_jnl_blocks[0..15] == journal i_block, [16] == i_size_lo",
          sjnl[:60] == raw[0x28:0x64] and u32(sjnl, 64) == u32(raw, 0x04))
    print(f"note s_jnl_blocks[15] (i_size_high slot 0x148) = {u32(sjnl, 60):#x}")
    check("s_journal_uuid all zero (internal journal)", not any(fs.sb[0xD0:0xE0]))


def check_bitmap_padding(fs):
    d = fs.desc(fs.ngroups - 1)
    bb = fs.blk(u32(d, 0x00))
    last_blocks = fs.blocks_count - (fs.ngroups - 1) * fs.bpg
    used_bytes = (last_blocks + 7) // 8
    pad_ok = all(b == 0xFF for b in bb[used_bytes:])
    if last_blocks % 8:
        mask = (0xFF << (last_blocks % 8)) & 0xFF
        pad_ok = pad_ok and (bb[used_bytes - 1] & mask) == mask
    check(f"last group block bitmap padding bits set to 1 ({last_blocks} blocks)", pad_ok)
    # inode bitmap padding: 0xff beyond ipg/8 — but INODE_UNINIT groups get an
    # entirely zero bitmap block (content ignored, csum stored as 0)
    for g in (0, fs.ngroups - 1):
        d = fs.desc(g)
        ib = fs.blk(u32(d, 0x04))
        if u16(d, 0x12) & 0x1:
            check(f"g{g} INODE_UNINIT: inode bitmap block all-zero", not any(ib))
        else:
            check(f"g{g} inode bitmap padding beyond ipg/8 set to 1",
                  all(b == 0xFF for b in ib[(fs.ipg + 7) // 8:]))


def run_checks(path, dxpath=None):
    fs = Fs(path)
    print(f"--- {path}: {fs.blocks_count} blocks, {fs.ngroups} groups, ipg {fs.ipg}, "
          f"seed {fs.seed:#010x}")
    check_superblock(fs)
    check_group_descs(fs)
    check_reserved_inodes(fs)
    check_root_dir_block(fs)
    check_empty_dir_blocks(fs)
    check_bitmap_padding(fs)
    check_journal(fs)
    for p, fn in (("/empty", check_empty_file), ("/sym_59", lambda f, _: check_symlinks(f)),
                  ("/dev_c_old", lambda f, _: check_devices(f)),
                  ("/xattr_block", lambda f, _: check_xattr_block(f)),
                  ("/xattr_mixed", lambda f, _: check_xattr_ibody(f)),
                  ("/big600", lambda f, _: check_extent_tree(f, "/big600"))):
        try:
            fs.resolve(p)
        except KeyError:
            continue
        fn(fs, p)
    if dxpath:
        fsx = Fs(dxpath)
        print(f"--- {dxpath} (htree variant)")
        for d in ("/bigdir", "/hugedir"):
            try:
                fsx.resolve(d)
            except KeyError:
                continue
            check_htree(fsx, d)


# --- vector dump -------------------------------------------------------------

def dump_vectors(path, dxpath, outdir):
    import os
    fs = Fs(path)
    os.makedirs(outdir, exist_ok=True)
    manifest = {"source": "mke2fs 1.47.4 via tools/mkrefs.sh",
                "image": os.path.basename(path), "uuid": fs.uuid.hex(),
                "hash_seed": [f"{x:#010x}" for x in fs.hash_seed],
                "fs_csum_seed": f"{fs.seed:#010x}", "blobs": {}}

    def put(name, data, **meta):
        with open(os.path.join(outdir, name), "wb") as f:
            f.write(data)
        manifest["blobs"][name] = meta | {"len": len(data)}

    put("sb_primary.bin", fs.sb, offset=1024)
    put("sb_backup_g1.bin", fs.blk(fs.bpg)[:1024], offset=fs.bpg * BS)
    ngdt = (fs.ngroups * 32 + BS - 1) // BS
    put("gdt.bin", fs.blk(1, ngdt)[:fs.ngroups * 32], offset=BS, groups=fs.ngroups)
    for g in range(min(fs.ngroups, 2)):
        d = fs.desc(g)
        put(f"block_bitmap_g{g}.bin", fs.blk(u32(d, 0)), offset=u32(d, 0) * BS)
        put(f"inode_bitmap_g{g}.bin", fs.blk(u32(d, 4)), offset=u32(d, 4) * BS)
    for ino, tag in ((1, "bad_blocks"), (2, "root"), (8, "journal"), (11, "lost_found")):
        raw, off = fs.inode_raw(ino)
        put(f"inode_{ino}_{tag}.bin", raw, offset=off, ino=ino)
    for p in ("/small.txt", "/empty", "/sym_59", "/sym_60", "/dev_c_old", "/dev_c_new",
              "/xattr_ibody", "/xattr_mixed", "/xattr_block", "/sparse_small"):
        try:
            ino = fs.resolve(p)
        except KeyError:
            continue
        raw, off = fs.inode_raw(ino)
        put(f"inode_{p.strip('/').replace('.', '_')}.bin", raw, offset=off, ino=ino, path=p)
    root_raw, _ = fs.inode_raw(2)
    rb = fs.file_block(root_raw, 0)
    put("dirblock_root.bin", fs.blk(rb), offset=rb * BS, ino=2)
    lf = fs.resolve("/lost+found")
    lf_raw, _ = fs.inode_raw(lf)
    eb = fs.file_block(lf_raw, 1)
    put("dirblock_empty.bin", fs.blk(eb), offset=eb * BS, ino=lf)
    xi = fs.resolve("/xattr_block")
    xraw, _ = fs.inode_raw(xi)
    acl = u32(xraw, 0x68)
    put("xattr_block.bin", fs.blk(acl), offset=acl * BS, ino=xi, block=acl)
    jraw, _ = fs.inode_raw(8)
    jb = fs.file_block(jraw, 0)
    put("jsb.bin", fs.blk(jb)[:1024], offset=jb * BS)
    if dxpath:
        fsx = Fs(dxpath)
        di = fsx.resolve("/bigdir")
        draw, _ = fsx.inode_raw(di)
        b0 = fsx.file_block(draw, 0)
        b1 = fsx.file_block(draw, 1)
        put("dx_root_bigdir.bin", fsx.blk(b0), offset=b0 * BS, ino=di,
            image=dxpath.split("/")[-1])
        put("dx_leaf_bigdir.bin", fsx.blk(b1), offset=b1 * BS, ino=di,
            image=dxpath.split("/")[-1])
        draw_raw, _ = fsx.inode_raw(di)
        put("inode_bigdir_dx.bin", draw_raw, ino=di, image=dxpath.split("/")[-1])
    with open(os.path.join(outdir, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=1, sort_keys=True)
    print(f"wrote {len(manifest['blobs'])} blobs + manifest.json to {outdir}")


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "check"
    if mode == "check":
        run_checks(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
        print(f"\n{len(FAILURES)} failures" if FAILURES else "\nall checks passed")
        sys.exit(1 if FAILURES else 0)
    elif mode == "dump":
        dump_vectors(sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        sys.exit(__doc__)
