//! Magic numbers, feature bits, and fixed geometry constants.

/// The only block size this crate emits (readers also require it).
pub const BLOCK_SIZE: usize = 4096;
/// Inode size the writer emits (`extra_isize` features require ≥ 256).
pub const INODE_SIZE: usize = 256;
/// Blocks per group at 4096-byte blocks (8 × block size).
pub const BLOCKS_PER_GROUP: u32 = 32768;
/// Superblock magic (`s_magic`).
pub const SB_MAGIC: u16 = 0xEF53;
/// Extent tree node magic (`eh_magic`).
pub const EXTENT_MAGIC: u16 = 0xF30A;
/// xattr header magic, both in-inode and block (`h_magic`).
pub const XATTR_MAGIC: u32 = 0xEA02_0000;
/// jbd2 journal superblock magic (big-endian on disk).
pub const JBD2_MAGIC: u32 = 0xC03B_3998;
/// First non-reserved inode (`s_first_ino`).
pub const FIRST_INO: u32 = 11;
/// The root directory inode.
pub const ROOT_INO: u32 = 2;
/// The journal inode.
pub const JOURNAL_INO: u32 = 8;
/// Maximum hard links (files error above this; dirs overflow to nlink 1).
pub const LINK_MAX: u32 = 65_000;

/// `s_feature_compat` bits.
pub mod compat {
    /// has_journal
    pub const HAS_JOURNAL: u32 = 0x4;
    /// ext_attr
    pub const EXT_ATTR: u32 = 0x8;
    /// dir_index (htree)
    pub const DIR_INDEX: u32 = 0x20;
    /// Everything the writer sets.
    pub const WRITER: u32 = HAS_JOURNAL | EXT_ATTR | DIR_INDEX;
}

/// `s_feature_incompat` bits.
pub mod incompat {
    /// filetype (dirent type byte)
    pub const FILETYPE: u32 = 0x2;
    /// journal needs recovery — set by an unclean mount, never by us.
    pub const RECOVER: u32 = 0x4;
    /// extents
    pub const EXTENTS: u32 = 0x40;
    /// 64-bit block numbers (read-supported, never written)
    pub const BIT64: u32 = 0x80;
    /// flex_bg
    pub const FLEX_BG: u32 = 0x200;
    /// checksum seed stored in the superblock (read-supported)
    pub const CSUM_SEED: u32 = 0x2000;
    /// Everything the writer sets.
    pub const WRITER: u32 = FILETYPE | EXTENTS | FLEX_BG;
    /// Everything the reader understands.
    pub const READER: u32 = FILETYPE | EXTENTS | BIT64 | FLEX_BG | CSUM_SEED;
}

/// `s_feature_ro_compat` bits.
pub mod ro_compat {
    /// sparse_super
    pub const SPARSE_SUPER: u32 = 0x1;
    /// large_file
    pub const LARGE_FILE: u32 = 0x2;
    /// huge_file
    pub const HUGE_FILE: u32 = 0x8;
    /// gdt_csum — mutually exclusive with metadata_csum.
    pub const GDT_CSUM: u32 = 0x10;
    /// dir_nlink
    pub const DIR_NLINK: u32 = 0x20;
    /// extra_isize
    pub const EXTRA_ISIZE: u32 = 0x40;
    /// metadata_csum
    pub const METADATA_CSUM: u32 = 0x400;
    /// Everything the writer sets.
    pub const WRITER: u32 =
        SPARSE_SUPER | LARGE_FILE | HUGE_FILE | DIR_NLINK | EXTRA_ISIZE | METADATA_CSUM;
}

/// `i_flags` bits used by this crate.
pub mod iflags {
    /// EXT4_INDEX_FL — hash-indexed directory.
    pub const INDEX: u32 = 0x1000;
    /// EXT4_EXTENTS_FL — inode uses extents.
    pub const EXTENTS: u32 = 0x8_0000;
}

/// `bg_flags` bits.
pub mod bg_flags {
    /// Inode table/bitmap unused (all inodes free).
    pub const INODE_UNINIT: u16 = 0x1;
    /// Block bitmap not initialized (read-supported, never written).
    pub const BLOCK_UNINIT: u16 = 0x2;
    /// Inode table is zeroed.
    pub const INODE_ZEROED: u16 = 0x4;
}

/// Dirent `file_type` byte values.
pub mod file_type {
    /// Regular file.
    pub const REG: u8 = 1;
    /// Directory.
    pub const DIR: u8 = 2;
    /// Character device.
    pub const CHR: u8 = 3;
    /// Block device.
    pub const BLK: u8 = 4;
    /// FIFO.
    pub const FIFO: u8 = 5;
    /// Unix socket.
    pub const SOCK: u8 = 6;
    /// Symbolic link.
    pub const SYMLINK: u8 = 7;
}
