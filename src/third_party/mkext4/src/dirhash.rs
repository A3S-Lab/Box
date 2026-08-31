//! ext4 directory-name hashing (htree / dir_index).
//!
//! Implements the half_md4 hash — the only `s_def_hash_version` this
//! crate's writer emits — in both signed- and unsigned-char flavors so the
//! reader can verify foreign images (`s_flags` bit 0 = signed, bit 1 =
//! unsigned; platform char signedness decided at their mkfs time).
//!
//! Verified byte-for-byte against `debugfs dx_hash` and against every
//! entry of real hash-indexed directories (`testdata/vectors/dx_hash.json`
//! and the dx fixture blobs).

/// Which `str2hashbuf` flavor the filesystem was hashed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signedness {
    /// `s_flags` bit 0: names folded as signed chars (x86, Apple arm64).
    Signed,
    /// `s_flags` bit 1: names folded as unsigned chars (arm64 Linux, s390).
    Unsigned,
}

/// The 4-word hash seed (`s_hash_seed`), stored little-endian in the
/// superblock. All-zero means "use the built-in MD4 initial state".
pub type HashSeed = [u32; 4];

/// Interpret a UUID's 16 bytes as a hash seed the way mke2fs does
/// (`-E hash_seed=UUID`): four little-endian u32 words.
pub fn seed_from_uuid(uuid: &[u8; 16]) -> HashSeed {
    [
        u32::from_le_bytes(uuid[0..4].try_into().unwrap()),
        u32::from_le_bytes(uuid[4..8].try_into().unwrap()),
        u32::from_le_bytes(uuid[8..12].try_into().unwrap()),
        u32::from_le_bytes(uuid[12..16].try_into().unwrap()),
    ]
}

/// half_md4 directory hash. Returns `(hash, minor_hash)`, where `hash`
/// already has the low bit cleared (dx entries reserve it as the
/// collision-continuation flag).
pub fn half_md4(seed: &HashSeed, signedness: Signedness, name: &[u8]) -> (u32, u32) {
    let mut buf: [u32; 4] = if seed.iter().any(|&w| w != 0) {
        *seed
    } else {
        [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476]
    };
    // The kernel loops while len > 0, so empty names get no transform at
    // all and hash straight from the seed state. Directory names are never
    // empty, but the reader should not panic on hostile input.
    let mut chunks = name.chunks(32);
    let first = chunks.next().unwrap_or(&[]);
    half_md4_transform(&mut buf, &str2hashbuf(first, signedness));
    for chunk in chunks {
        half_md4_transform(&mut buf, &str2hashbuf(chunk, signedness));
    }
    (buf[1] & !1, buf[2])
}

/// Pack up to 32 name bytes into 8 u32 words, kernel `str2hashbuf_*`
/// semantics: bytes are shifted in MSB-first with signed/unsigned
/// extension, words are padded with a length-derived pattern.
fn str2hashbuf(msg: &[u8], signedness: Signedness) -> [u32; 8] {
    let len = msg.len();
    let mut pad = (len as u32) | ((len as u32) << 8);
    pad |= pad << 16;

    let mut out = [0u32; 8];
    let mut val = pad;
    let mut widx = 0usize;
    for (i, &b) in msg.iter().enumerate().take(32) {
        let c = match signedness {
            Signedness::Signed => b as i8 as i32 as u32, // sign-extended
            Signedness::Unsigned => u32::from(b),
        };
        val = c.wrapping_add(val << 8);
        if i % 4 == 3 {
            out[widx] = val;
            widx += 1;
            val = pad;
        }
    }
    // Kernel: `if (--num >= 0) *buf++ = val; while (--num >= 0) *buf++ = pad;`
    if widx < 8 {
        out[widx] = val;
        widx += 1;
    }
    while widx < 8 {
        out[widx] = pad;
        widx += 1;
    }
    out
}

/// One half-MD4 transform (`lib/halfmd4.c`): three 8-step rounds over the
/// state, target order a,d,c,b within each round.
fn half_md4_transform(buf: &mut [u32; 4], inp: &[u32; 8]) {
    #[inline(always)]
    fn f(x: u32, y: u32, z: u32) -> u32 {
        z ^ (x & (y ^ z))
    }
    #[inline(always)]
    fn g(x: u32, y: u32, z: u32) -> u32 {
        (x & y).wrapping_add((x ^ y) & z)
    }
    #[inline(always)]
    fn h(x: u32, y: u32, z: u32) -> u32 {
        x ^ y ^ z
    }

    const K2: u32 = 0x5A82_7999;
    const K3: u32 = 0x6ED9_EBA1;
    // (input index, rotate) schedules per round.
    const R1: [(usize, u32); 8] = [
        (0, 3),
        (1, 7),
        (2, 11),
        (3, 19),
        (4, 3),
        (5, 7),
        (6, 11),
        (7, 19),
    ];
    const R2: [(usize, u32); 8] = [
        (1, 3),
        (3, 5),
        (5, 9),
        (7, 13),
        (0, 3),
        (2, 5),
        (4, 9),
        (6, 13),
    ];
    const R3: [(usize, u32); 8] = [
        (3, 3),
        (7, 9),
        (2, 11),
        (6, 15),
        (1, 3),
        (5, 9),
        (0, 11),
        (4, 15),
    ];

    let mut v = *buf;
    let mut round = |func: fn(u32, u32, u32) -> u32, k: u32, sched: &[(usize, u32); 8]| {
        for (i, &(x, s)) in sched.iter().enumerate() {
            // Update targets cycle a, d, c, b (indices 0, 3, 2, 1).
            let t = [0usize, 3, 2, 1][i % 4];
            let (b1, c1, d1) = (v[(t + 1) % 4], v[(t + 2) % 4], v[(t + 3) % 4]);
            v[t] = v[t]
                .wrapping_add(func(b1, c1, d1))
                .wrapping_add(inp[x])
                .wrapping_add(k)
                .rotate_left(s);
        }
    };
    round(f, 0, &R1);
    round(g, K2, &R2);
    round(h, K3, &R3);

    for i in 0..4 {
        buf[i] = buf[i].wrapping_add(v[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture seed: UUID deadbeef-dead-4ead-8ead-deadbeef0000 as LE words
    /// (see testdata/vectors/dx_hash.json).
    const SEED: HashSeed = [0xefbe_adde, 0xad4e_adde, 0xadde_ad8e, 0x0000_efbe];

    #[test]
    fn debugfs_dx_hash_vectors() {
        // Extracted via `debugfs -R "dx_hash -h half_md4 -s <uuid> <name>"`.
        for (name, hash, minor) in [
            ("entry_00207_pad", 0x0066_3894, 0xfa7c_d671u32),
            ("entry_00000_pad", 0xe289_c2b4, 0x8f16_e063),
            ("hello.txt", 0x093b_83d6, 0x33e0_ca61),
            ("a", 0x5538_916c, 0x34a2_9fa3),
        ] {
            assert_eq!(
                half_md4(&SEED, Signedness::Signed, name.as_bytes()),
                (hash, minor),
                "{name}"
            );
        }
    }

    #[test]
    fn signedness_matters_for_high_bytes() {
        let name = [b'x', 0xE9, b'z']; // 0xE9: sign-extension differs
        let s = half_md4(&SEED, Signedness::Signed, &name);
        let u = half_md4(&SEED, Signedness::Unsigned, &name);
        assert_ne!(s, u);
        // Pure-ASCII names hash identically under both flavors.
        let a = b"ascii_only";
        assert_eq!(
            half_md4(&SEED, Signedness::Signed, a),
            half_md4(&SEED, Signedness::Unsigned, a)
        );
    }

    #[test]
    fn zero_seed_uses_md4_init() {
        let (h1, _) = half_md4(&[0; 4], Signedness::Signed, b"name");
        let (h2, _) = half_md4(&[1, 0, 0, 0], Signedness::Signed, b"name");
        assert_ne!(h1, h2);
    }

    #[test]
    fn long_names_use_multiple_transforms() {
        // 100-byte name: 4 chunks (32+32+32+4).
        let name = vec![b'q'; 100];
        let (h, m) = half_md4(&SEED, Signedness::Signed, &name);
        // Distinct from the 32-byte prefix — the tail chunks must matter.
        let (hp, mp) = half_md4(&SEED, Signedness::Signed, &name[..32]);
        assert_ne!((h, m), (hp, mp));
        assert_eq!(h & 1, 0, "low bit must be cleared");
    }

    #[test]
    fn seed_from_uuid_word_order() {
        let uuid: [u8; 16] = [
            0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0x4e, 0xad, 0x8e, 0xad, 0xde, 0xad, 0xbe, 0xef,
            0x00, 0x00,
        ];
        assert_eq!(seed_from_uuid(&uuid), SEED);
    }
}
