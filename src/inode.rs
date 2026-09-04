//! ext4 inode parsing.
//!
//! Spec: docs/ext4-spec/inodes-extents.md
//!
//! Base inode is 128 bytes; modern ext4 with EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE
//! adds another 32 bytes (i_extra_isize) for a total of 160 bytes. All fields
//! little-endian. The high halves of uid/gid/size/file_acl/blocks/checksum live
//! at the end of the base 128 bytes; nanosecond timestamps + crtime live in the
//! extra section.

use crate::error::{Error, Result};

/// Minimum on-disk inode size (rev 0).
pub const INODE_BASE_SIZE: usize = 128;
/// The size of the original ext2 inode — `EXT2_GOOD_OLD_INODE_SIZE`.
///
/// Everything up to here is the fixed part every ext2/3/4 inode has;
/// anything past it is the `i_extra_isize` region, which only larger
/// inodes carry. So it is the length below which an inode buffer cannot
/// hold `i_checksum_lo` at 0x7C, and the point at which
/// `Checksummer::verify_inode` refuses.
///
/// It was declared in `mkfs.rs`, unused, while `checksum.rs` wrote the
/// bare `128` twice.
pub const GOOD_OLD_INODE_SIZE: usize = 128;

/// Offset where the i_extra_isize field begins (start of extra section).
pub const INODE_EXTRA_OFFSET: usize = 128;

// Raw inode field byte offsets (from the start of the on-disk inode, little-endian).
// Named so build_*_inode helpers can write fields without requiring readers to
// memorise the ext4 spec layout. Source: docs/ext4-spec/inodes-extents.md.
pub(crate) const OFF_MODE: usize = 0x00;
pub(crate) const OFF_SIZE_LO: usize = 0x04;
pub(crate) const OFF_ATIME: usize = 0x08;
pub(crate) const OFF_CTIME: usize = 0x0C;
pub(crate) const OFF_MTIME: usize = 0x10;
pub(crate) const OFF_LINKS_COUNT: usize = 0x1A;
pub(crate) const OFF_BLOCKS_LO: usize = 0x1C;
pub(crate) const OFF_FLAGS: usize = 0x20;
pub(crate) const OFF_BLOCK: usize = 0x28; // i_block area start (60 bytes, 0x28..0x64)
pub(crate) const OFF_GENERATION: usize = 0x64;
pub(crate) const OFF_SIZE_HI: usize = 0x6C;
pub(crate) const OFF_BLOCKS_HI: usize = 0x74;
pub(crate) const OFF_CHECKSUM_LO: usize = 0x7C;
pub(crate) const OFF_EXTRA_ISIZE: usize = 0x80;
pub(crate) const OFF_CHECKSUM_HI: usize = 0x82;
pub(crate) const OFF_CRTIME: usize = 0x90;

/// Default i_extra_isize value written into new inodes: covers checksum_hi,
/// nsec timestamps, and i_crtime (32 bytes beyond the 128-byte base).
pub(crate) const EXTRA_ISIZE_DEFAULT: u16 = 32;
/// Minimum inode buffer length for i_crtime (offset 0x90) to be present.
pub(crate) const INODE_SIZE_WITH_CRTIME: usize = 0x94;
/// Minimum inode buffer length for i_extra_isize + i_checksum_hi.
pub(crate) const INODE_SIZE_WITH_EXTRA: usize = 0x84;

// POSIX file-type bits (high nibble of i_mode).
pub const S_IFMT: u16 = 0xF000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFLNK: u16 = 0xA000;
pub const S_IFBLK: u16 = 0x6000;
pub const S_IFCHR: u16 = 0x2000;
pub const S_IFIFO: u16 = 0x1000;
pub const S_IFSOCK: u16 = 0xC000;

bitflags::bitflags! {
    /// `i_flags` — per-inode behaviour flags.
    /// Spec: kernel.org/doc/html/latest/filesystems/ext4/inodes.html
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct InodeFlags: u32 {
        /// Secure deletion (unused).
        const SECRM        = 0x0000_0001;
        /// Undelete (unused).
        const UNRM         = 0x0000_0002;
        /// Compressed file.
        const COMPR        = 0x0000_0004;
        /// Synchronous writes.
        const SYNC         = 0x0000_0008;
        /// Immutable.
        const IMMUTABLE    = 0x0000_0010;
        /// Append-only.
        const APPEND       = 0x0000_0020;
        /// Do not dump.
        const NODUMP       = 0x0000_0040;
        /// Do not update access time.
        const NOATIME      = 0x0000_0080;
        /// Hash-tree-indexed directory.
        const INDEX        = 0x0000_1000;
        /// File data stored in extended attributes.
        const EA_INODE     = 0x0020_0000;
        /// Inode uses extents (EXT4_EXTENTS_FL).
        const EXTENTS      = 0x0008_0000;
        /// Inode stores a huge file (i_blocks counted in fs blocks not 512B sectors).
        const HUGE_FILE    = 0x0004_0000;
        /// Inline data — file contents live inside i_block + xattrs.
        const INLINE_DATA  = 0x1000_0000;
        /// Alias for EXTENTS (matches kernel naming `EXT4_EXTENTS_FL`).
        const EXTENT       = 0x0008_0000;
        /// Inode has extra (nanosecond) timestamp fields.
        const EXTRA_ATIME  = 0x0000_0100;
    }
}

/// Parsed ext4 inode.
///
/// Combines hi+lo halves for uid, gid, size, file_acl, blocks, and checksum so
/// callers don't have to reassemble them. Nanosecond timestamps come from the
/// `*_extra` fields when present (top 30 bits = nsec, low 2 bits = epoch).
#[derive(Debug, Clone)]
pub struct Inode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    /// Seconds since the Unix epoch, **signed and 64-bit**.
    ///
    /// The on-disk base field is a signed 32-bit value, so dates before
    /// 1970 are representable and must not be read as far-future ones.
    /// When `i_extra_isize` is large enough, the low two bits of the
    /// matching `*_extra` field extend the seconds by `<< 32`, widening
    /// the range from 1901..2038 to roughly 1901..2446. Both are
    /// applied here; see `decode_extra_time`.
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub dtime: i64,
    pub crtime: i64,
    pub atime_nsec: u32,
    pub mtime_nsec: u32,
    pub ctime_nsec: u32,
    pub crtime_nsec: u32,
    pub links_count: u16,
    pub blocks: u64, // 512-byte sectors (per spec; HUGE_FILE flag changes meaning)
    pub flags: u32,
    /// Raw 60-byte i_block area — extent header / direct pointers / inline data.
    /// Parsed by the extent module.
    pub block: [u8; 60],
    pub generation: u32,
    pub file_acl: u64,
    pub checksum: u32,
}

/// Combine a base timestamp with its `*_extra` field.
///
/// ext4 stores seconds in two places once `i_extra_isize` is large
/// enough. The base field is a **signed** 32-bit count from the Unix
/// epoch — negative values are dates before 1970 and are legal. The
/// `*_extra` field packs two things: its **low two bits extend the
/// seconds by 2^32**, and the upper thirty are nanoseconds.
///
/// Reading only the base gives 1901..2038. Adding the two epoch bits
/// gives roughly 1901..2446, which is what the format actually means.
/// The nanosecond half was already being read (`extra >> 2`); the
/// epoch half was discarded, so every timestamp past 2038 came back
/// 136 years early.
///
/// Matches `ext4_decode_extra_time` in `fs/ext4/ext4.h`.
fn decode_extra_time(base: u32, extra: u32) -> i64 {
    // The base is signed on disk: reinterpret before widening, or a
    // pre-1970 date becomes a date in 2106.
    let secs = base as i32 as i64;
    let epoch_bits = (extra & EXT4_EPOCH_MASK) as i64;
    secs + (epoch_bits << 32)
}

/// Low two bits of an `*_extra` field: the seconds extension.
const EXT4_EPOCH_MASK: u32 = 0x3;

/// The inverse of [`decode_extra_time`]: split a POSIX seconds value
/// into the on-disk base and the two epoch bits that belong in the low
/// end of the matching `*_extra` field.
///
/// Matches `ext4_encode_extra_time` in `fs/ext4/ext4.h`:
///
/// ```c
/// extra = ((time->tv_sec - (s32)time->tv_sec) >> 32) & EXT4_EPOCH_MASK;
/// ```
///
/// The epoch bits account for the **signed** reinterpretation of the
/// base, not merely for the bits above 32. 2100-01-01 is 4102444800,
/// which fits in a `u32` but is negative as an `i32` — so it is stored
/// as that negative base *plus* an epoch of 1, and the two cancel back
/// to the right answer. Splitting the value at bit 32 instead would
/// compute an epoch of 0 and store the wrong date.
pub(crate) fn encode_extra_time(secs: i64) -> (u32, u32) {
    let base = secs as u32;
    let epoch = (((secs - (secs as i32 as i64)) >> 32) as u32) & EXT4_EPOCH_MASK;
    (base, epoch)
}

/// The range [`encode_extra_time`] can represent: a signed 32-bit base
/// plus two epoch bits, so 1901-12-13 through 2446-05-10. A caller
/// asking to store a time outside this is asking for something the
/// format cannot hold.
pub(crate) const MIN_ENCODABLE_TIME: i64 = i32::MIN as i64;
pub(crate) const MAX_ENCODABLE_TIME: i64 = i32::MAX as i64 + (3i64 << 32);

impl Inode {
    /// Parse an inode from its on-disk bytes.
    /// Accepts any length >= 128; if >= 160 and i_extra_isize >= 28, parses the
    /// extra (nsec + crtime + checksum_hi) section as well.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() < INODE_BASE_SIZE {
            return Err(Error::Corrupt("inode buffer too small"));
        }

        let mode = u16::from_le_bytes(raw[OFF_MODE..OFF_MODE + 2].try_into().unwrap());
        let uid_lo = u16::from_le_bytes(raw[0x02..0x04].try_into().unwrap());
        let size_lo = u32::from_le_bytes(raw[OFF_SIZE_LO..OFF_SIZE_LO + 4].try_into().unwrap());
        let atime_base = u32::from_le_bytes(raw[OFF_ATIME..OFF_ATIME + 4].try_into().unwrap());
        let ctime_base = u32::from_le_bytes(raw[OFF_CTIME..OFF_CTIME + 4].try_into().unwrap());
        let mtime_base = u32::from_le_bytes(raw[OFF_MTIME..OFF_MTIME + 4].try_into().unwrap());
        let dtime = u32::from_le_bytes(raw[0x14..0x18].try_into().unwrap());
        let gid_lo = u16::from_le_bytes(raw[0x18..0x1A].try_into().unwrap());
        let links_count = u16::from_le_bytes(
            raw[OFF_LINKS_COUNT..OFF_LINKS_COUNT + 2]
                .try_into()
                .unwrap(),
        );
        let blocks_lo =
            u32::from_le_bytes(raw[OFF_BLOCKS_LO..OFF_BLOCKS_LO + 4].try_into().unwrap());
        let flags = u32::from_le_bytes(raw[OFF_FLAGS..OFF_FLAGS + 4].try_into().unwrap());
        // 0x24..0x28 is i_osd1 (Linux: i_version_lo) — ignored here.

        let mut block = [0u8; 60];
        block.copy_from_slice(&raw[OFF_BLOCK..OFF_BLOCK + 60]);

        let generation =
            u32::from_le_bytes(raw[OFF_GENERATION..OFF_GENERATION + 4].try_into().unwrap());
        let file_acl_lo = u32::from_le_bytes(raw[0x68..0x6C].try_into().unwrap());
        let size_hi = u32::from_le_bytes(raw[OFF_SIZE_HI..OFF_SIZE_HI + 4].try_into().unwrap());
        // 0x70..0x74 obso_faddr ignored.
        let blocks_hi =
            u16::from_le_bytes(raw[OFF_BLOCKS_HI..OFF_BLOCKS_HI + 2].try_into().unwrap());
        let file_acl_hi = u16::from_le_bytes(raw[0x76..0x78].try_into().unwrap());
        let uid_hi = u16::from_le_bytes(raw[0x78..0x7A].try_into().unwrap());
        let gid_hi = u16::from_le_bytes(raw[0x7A..0x7C].try_into().unwrap());
        let checksum_lo = u16::from_le_bytes(
            raw[OFF_CHECKSUM_LO..OFF_CHECKSUM_LO + 2]
                .try_into()
                .unwrap(),
        );
        // 0x7E..0x80 i_reserved2.

        // Defaults (when no extra section present).
        let mut atime_nsec = 0u32;
        let mut mtime_nsec = 0u32;
        let mut ctime_nsec = 0u32;
        let mut crtime_nsec = 0u32;
        let mut crtime_base = 0u32;
        // The `*_extra` words, zero when i_extra_isize is too small to
        // hold them — which correctly yields no epoch extension.
        let mut atime_extra = 0u32;
        let mut mtime_extra = 0u32;
        let mut ctime_extra = 0u32;
        let mut crtime_extra = 0u32;
        let mut checksum_hi = 0u16;

        // Extra fields — only present when on-disk inode size is >= 160 AND
        // i_extra_isize covers them (>= 28 includes through i_projid; we read
        // what we need at >= 24 to cover up to crtime_extra).
        if raw.len() >= INODE_EXTRA_OFFSET + 4 {
            let i_extra_isize = u16::from_le_bytes(
                raw[OFF_EXTRA_ISIZE..OFF_EXTRA_ISIZE + 2]
                    .try_into()
                    .unwrap(),
            );
            // Sanity: i_extra_isize is the number of bytes beyond the 128-byte
            // base that are valid. Must fit inside the on-disk inode.
            let extra_end = INODE_EXTRA_OFFSET + i_extra_isize as usize;
            if extra_end > raw.len() {
                return Err(Error::Corrupt("i_extra_isize exceeds inode size"));
            }

            // Read each extra field only if i_extra_isize covers it.
            // Layout (offset from inode start):
            //   0x80 u16 i_extra_isize
            //   0x82 u16 i_checksum_hi          (needs >= 4)
            //   0x84 u32 i_ctime_extra          (needs >= 8)
            //   0x88 u32 i_mtime_extra          (needs >= 12)
            //   0x8C u32 i_atime_extra          (needs >= 16)
            //   0x90 u32 i_crtime               (needs >= 20)
            //   0x94 u32 i_crtime_extra         (needs >= 24)
            if i_extra_isize >= 4 {
                checksum_hi = u16::from_le_bytes(
                    raw[OFF_CHECKSUM_HI..OFF_CHECKSUM_HI + 2]
                        .try_into()
                        .unwrap(),
                );
            }
            if i_extra_isize >= 8 {
                let extra = u32::from_le_bytes(raw[0x84..0x88].try_into().unwrap());
                ctime_nsec = extra >> 2;
                ctime_extra = extra;
            }
            if i_extra_isize >= 12 {
                let extra = u32::from_le_bytes(raw[0x88..0x8C].try_into().unwrap());
                mtime_nsec = extra >> 2;
                mtime_extra = extra;
            }
            if i_extra_isize >= 16 {
                let extra = u32::from_le_bytes(raw[0x8C..0x90].try_into().unwrap());
                atime_nsec = extra >> 2;
                atime_extra = extra;
            }
            if i_extra_isize >= 20 {
                crtime_base =
                    u32::from_le_bytes(raw[OFF_CRTIME..OFF_CRTIME + 4].try_into().unwrap());
            }
            if i_extra_isize >= 24 {
                let extra = u32::from_le_bytes(raw[0x94..0x98].try_into().unwrap());
                crtime_nsec = extra >> 2;
                crtime_extra = extra;
            }
        }

        Ok(Self {
            mode,
            uid: join16(uid_hi, uid_lo),
            gid: join16(gid_hi, gid_lo),
            size: join32(size_hi, size_lo),
            atime: decode_extra_time(atime_base, atime_extra),
            mtime: decode_extra_time(mtime_base, mtime_extra),
            ctime: decode_extra_time(ctime_base, ctime_extra),
            // dtime has no *_extra field in the format: deletion time
            // is a plain signed 32-bit value with no epoch extension.
            dtime: dtime as i32 as i64,
            crtime: decode_extra_time(crtime_base, crtime_extra),
            atime_nsec,
            mtime_nsec,
            ctime_nsec,
            crtime_nsec,
            links_count,
            blocks: join32(blocks_hi, blocks_lo),
            flags,
            block,
            generation,
            file_acl: join32(file_acl_hi, file_acl_lo),
            checksum: join16(checksum_hi, checksum_lo),
        })
    }

    /// File type from i_mode.
    pub fn file_type(&self) -> u16 {
        self.mode & S_IFMT
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.file_type() == S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == S_IFLNK
    }

    /// True when EXT4_EXTENTS_FL is set in i_flags — i_block holds an extent
    /// tree rather than legacy direct/indirect block pointers.
    pub fn has_extents(&self) -> bool {
        self.flags & InodeFlags::EXTENTS.bits() != 0
    }

    /// True when INLINE_DATA flag is set — file contents live inside i_block.
    pub fn has_inline_data(&self) -> bool {
        self.flags & InodeFlags::INLINE_DATA.bits() != 0
    }

    /// Decode i_flags into a typed bitflags value (silently drops unknown bits).
    pub fn flag_set(&self) -> InodeFlags {
        InodeFlags::from_bits_truncate(self.flags)
    }
}

/// Combine two 16-bit halves into a 32-bit value (hi occupies the upper 16 bits).
/// Used when the on-disk layout stores a 32-bit field split across two u16 words.
#[inline]
fn join16(hi: u16, lo: u16) -> u32 {
    ((hi as u32) << 16) | lo as u32
}

/// Combine a hi half (any type that fits in u64) and a 32-bit lo half into a
/// 64-bit value. Used for size, file_acl, and i_blocks whose hi halves have
/// different widths (u16 or u32) in the on-disk layout.
#[inline]
fn join32<H: Into<u64>>(hi: H, lo: u32) -> u64 {
    (hi.into() << 32) | lo as u64
}

#[cfg(test)]
mod timestamp_tests {
    use super::decode_extra_time;

    /// With no `*_extra` field, a timestamp is the plain signed
    /// 32-bit value — the pre-2038 behaviour, unchanged.
    #[test]
    fn without_an_extra_field_the_base_is_used_as_is() {
        assert_eq!(decode_extra_time(0, 0), 0);
        assert_eq!(decode_extra_time(946_684_800, 0), 946_684_800);
    }

    /// **The base field is signed.** A value with the top bit set is a
    /// date before 1970, not a date in 2106. Reading it as `u32` was
    /// the second half of this bug.
    #[test]
    fn a_pre_1970_timestamp_stays_negative() {
        // -1 as a u32 bit pattern: 1969-12-31T23:59:59Z.
        assert_eq!(decode_extra_time(0xFFFF_FFFF, 0), -1);
        // 1901-12-13, the earliest a signed 32-bit count reaches.
        assert_eq!(decode_extra_time(0x8000_0000, 0), i32::MIN as i64);
    }

    /// **The fix.** The low two bits of `*_extra` extend the seconds
    /// by 2^32 each, moving the ceiling from 2038 to roughly 2446.
    ///
    /// Previously these bits were discarded by the `>> 2` that
    /// extracts nanoseconds, so every timestamp past 2038 came back
    /// 136 years early.
    #[test]
    fn the_epoch_bits_extend_the_range_past_2038() {
        // epoch=1 adds 2^32 seconds.
        assert_eq!(decode_extra_time(0, 0b01), 1i64 << 32);
        assert_eq!(decode_extra_time(0, 0b10), 2i64 << 32);
        assert_eq!(decode_extra_time(0, 0b11), 3i64 << 32);
    }

    /// The nanosecond bits must not leak into the seconds. `*_extra`
    /// packs both, and only the low two bits are the epoch.
    #[test]
    fn the_nanosecond_bits_do_not_affect_the_seconds() {
        // All thirty nsec bits set, epoch bits clear.
        let nsec_only = 0xFFFF_FFFCu32;
        assert_eq!(
            decode_extra_time(1_000, nsec_only),
            1_000,
            "nanoseconds must not be added to the seconds"
        );
    }

    /// A real post-2038 timestamp round-trips.
    ///
    /// Encoded the way the kernel does it, which is subtler than
    /// splitting the value at bit 32:
    ///
    /// ```c
    /// extra = ((time->tv_sec - (s32)time->tv_sec) >> 32) & EXT4_EPOCH_MASK;
    /// ```
    ///
    /// The epoch bits account for the **signed** reinterpretation of
    /// the base, not merely for bits above 32. 2100-01-01 is
    /// 4102444800, which fits in a `u32` but is negative as an `i32` —
    /// so it is stored as that negative base *plus* an epoch of 1, and
    /// the two cancel back to the right answer. A test that split at
    /// bit 32 would compute epoch=0 and assert the wrong encoding.
    use super::encode_extra_time as encode;

    #[test]
    fn a_date_in_2100_decodes_correctly() {
        const SECS_2100: i64 = 4_102_444_800;
        let (base, epoch) = encode(SECS_2100);
        assert_eq!(epoch, 1, "2100 needs the epoch extension");
        assert_eq!(
            decode_extra_time(base, epoch),
            SECS_2100,
            "a date in 2100 must not come back 136 years early"
        );
    }

    /// Round-trip across the interesting boundaries, so the encoder
    /// and decoder are checked against each other rather than against
    /// hand-computed constants.
    #[test]
    fn timestamps_round_trip_across_the_2038_boundary() {
        for secs in [
            i32::MIN as i64,       // 1901
            -1,                    // 1969
            0,                     // 1970
            946_684_800,           // 2000
            i32::MAX as i64,       // 2038-01-19, the old ceiling
            i32::MAX as i64 + 1,   // one second past it
            4_102_444_800,         // 2100
            (1i64 << 33) + 12_345, // needs both epoch bits
        ] {
            let (base, epoch) = encode(secs);
            assert_eq!(
                decode_extra_time(base, epoch),
                secs,
                "round trip for {secs}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join16_combines_halves() {
        assert_eq!(join16(0x0001, 0x0002), 0x0001_0002);
        assert_eq!(join16(0xFFFF, 0x0000), 0xFFFF_0000);
        assert_eq!(join16(0x0000, 0xFFFF), 0x0000_FFFF);
        assert_eq!(join16(0, 0), 0);
    }

    #[test]
    fn join32_combines_halves_u16_hi() {
        assert_eq!(join32(0x0001u16, 0x0000_0002), 0x0000_0001_0000_0002);
        assert_eq!(join32(0xFFFFu16, 0x0000_0000), 0x0000_FFFF_0000_0000);
        assert_eq!(join32(0x0000u16, 0xFFFF_FFFF), 0x0000_0000_FFFF_FFFF);
    }

    #[test]
    fn join32_combines_halves_u32_hi() {
        assert_eq!(join32(0x0000_0001u32, 0x0000_0002), 0x0000_0001_0000_0002);
        assert_eq!(join32(0xFFFF_FFFFu32, 0x0000_0000), 0xFFFF_FFFF_0000_0000);
    }

    #[test]
    fn parse_rejects_short_buffer() {
        let short = vec![0u8; 64];
        assert!(matches!(
            Inode::parse(&short),
            Err(crate::error::Error::Corrupt(_))
        ));
    }

    #[test]
    fn parse_rejects_invalid_extra_isize() {
        // 160-byte inode with i_extra_isize claiming 200 bytes (exceeds buffer).
        let mut raw = vec![0u8; 160];
        raw[0x80] = 200; // i_extra_isize lo byte — claims 200 bytes extra
        raw[0x81] = 0;
        assert!(matches!(
            Inode::parse(&raw),
            Err(crate::error::Error::Corrupt(_))
        ));
    }

    #[test]
    fn parse_mode_and_links_roundtrip() {
        let mut raw = vec![0u8; 128];
        raw[0x00..0x02].copy_from_slice(&0x81A4u16.to_le_bytes()); // S_IFREG | 0644
        raw[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes()); // links_count
        let inode = Inode::parse(&raw).unwrap();
        assert_eq!(inode.mode, 0x81A4);
        assert_eq!(inode.links_count, 3);
        assert!(inode.is_file());
    }
}
