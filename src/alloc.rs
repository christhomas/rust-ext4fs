//! Block + inode bitmap allocator — planning layer.
//!
//! Phase 4 write path scaffolding. This module produces typed
//! [`AllocationPlan`] values describing what bits to flip in which bitmap
//! block + the updated free-counter deltas. It does NOT write to disk;
//! E11 (journaled writes) will apply the plans atomically under a
//! JBD2 transaction.
//!
//! Rationale: separating allocation (pure function over bitmap bytes) from
//! commit (journaled block write) makes tests trivial and keeps the
//! read-only mount path untouched. Block device traits stay read-only in
//! Phase 1; the write trait lives at the commit boundary.
//!
//! ### Block bitmap layout
//! One bit per block in the group. Bit `i` = block `group_start + i`.
//! A 4 KiB block bitmap covers `32768` blocks (one block group on a
//! 4 KiB-block fs). Bits are packed LSB-first within each byte: bit 0 of
//! byte 0 represents the first block in the group.
//!
//! ### Inode bitmap layout
//! Same LSB-first packing. Bit `i` = inode `(group_idx * inodes_per_group) + i + 1`
//! (inode numbers are 1-based).
//!
//! ### Orlov allocator (directories)
//! Linux ext4 chooses a group for new directories using the Orlov heuristic:
//! prefer groups whose `(free_blocks, free_inodes, used_dirs)` triple is
//! "below average" — distributing directories evenly across groups so sibling
//! files end up near their parent dir. We implement a simplified variant:
//! iterate groups starting from `hint`, prefer one whose used_dirs is below
//! the fleet average and has the most free_inodes.

use crate::bgd::{BgdFlags, BlockGroupDescriptor};
use crate::error::{Error, Result};
use crate::superblock::Superblock;

/// A change to one bitmap block: flip bits `bit_start .. bit_start + count`
/// from 0 (free) to 1 (used). The new bitmap bytes are NOT materialised here —
/// only the semantic description is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapWrite {
    /// Physical block number of the bitmap (from `bg_block_bitmap` or
    /// `bg_inode_bitmap`).
    pub bitmap_block: u64,
    /// First bit index within this bitmap to flip.
    pub bit_start: u32,
    /// Number of consecutive bits to flip.
    pub count: u32,
    /// `true` if marking used, `false` if freeing.
    pub set: bool,
}

/// A change to one block-group descriptor's free-counter and/or
/// used_dirs_count. Applied together with the matching [`BitmapWrite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgdCounterUpdate {
    pub group_idx: u32,
    /// Delta to apply to `bg_free_blocks_count` (+free, -allocated).
    pub free_blocks_delta: i32,
    /// Delta to apply to `bg_free_inodes_count`.
    pub free_inodes_delta: i32,
    /// Delta to apply to `bg_used_dirs_count`.
    pub used_dirs_delta: i32,
}

/// A change to the superblock free-counter totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuperblockCounterUpdate {
    pub free_blocks_delta: i64,
    pub free_inodes_delta: i32,
}

/// Complete plan for one block-allocation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockAllocationPlan {
    /// First allocated fs block (absolute, not group-relative).
    pub first_block: u64,
    /// Number of contiguous blocks allocated.
    pub count: u32,
    pub bitmap: BitmapWrite,
    pub bgd: BgdCounterUpdate,
    pub sb: SuperblockCounterUpdate,
}

/// Complete plan for one inode-allocation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeAllocationPlan {
    /// Allocated inode number (1-based).
    pub inode: u32,
    /// True if this was a directory allocation (also bumps used_dirs).
    pub is_dir: bool,
    pub bitmap: BitmapWrite,
    pub bgd: BgdCounterUpdate,
    pub sb: SuperblockCounterUpdate,
}

// ---------------------------------------------------------------------------
// Pure bit-manipulation helpers — unit-testable without a device
// ---------------------------------------------------------------------------

/// Test bit `idx` in a bitmap (LSB-first within each byte).
#[inline]
pub fn bit_is_set(bitmap: &[u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let mask = 1u8 << (idx % 8);
    byte < bitmap.len() && bitmap[byte] & mask != 0
}

/// Find the first free (0-valued) bit at or after `start`, searching up to
/// `max_bits` total. Returns `None` if none found.
///
/// Fast path: once `start` is aligned to an 8-byte word, we scan the bitmap
/// as `u64`s and skip any word of all-ones in a single branch. On sparse
/// bitmaps (typical after mkfs) the scan is effectively memory-bandwidth
/// bound and ~8–16× faster than per-bit `bit_is_set`.
pub fn find_first_free(bitmap: &[u8], start: u32, max_bits: u32) -> Option<u32> {
    // A BIT THAT IS NOT IN THE BITMAP IS NOT A FREE BIT.
    //
    // `bit_is_set` answers "not set" for an index past the end of the
    // buffer, which reads as free, and `max_bits` comes from
    // `s_blocks_per_group` / `s_inodes_per_group` -- superblock fields
    // that nothing bounds to what a bitmap block can hold. So a group
    // claiming 2^31 inodes per group returned bit indices that are not
    // in its bitmap at all: the plan named a real inode or block, and
    // the write that was supposed to mark it used silently did nothing
    // while the counters were debited anyway. The next allocation then
    // returned the same one.
    let max_bits = max_bits.min(u32::try_from(bitmap.len().saturating_mul(8)).unwrap_or(u32::MAX));
    if start >= max_bits {
        return None;
    }
    let mut i = start;

    // 1) Scan to the next 64-bit-aligned bit boundary with the per-bit path.
    while i < max_bits && !i.is_multiple_of(64) {
        if !bit_is_set(bitmap, i) {
            return Some(i);
        }
        i += 1;
    }

    // 2) Word-at-a-time scan. Every word that is not `u64::MAX` has at least
    //    one zero bit; `trailing_ones` pinpoints the first one in LSB order
    //    (matching ext4's LSB-first within-byte convention).
    while i + 64 <= max_bits {
        let byte = (i as usize) / 8;
        if byte + 8 > bitmap.len() {
            break;
        }
        let word = u64::from_le_bytes(bitmap[byte..byte + 8].try_into().unwrap());
        if word != u64::MAX {
            let bit = word.trailing_ones();
            let cand = i + bit;
            // Guard against spurious max_bits boundary within the word.
            if cand < max_bits {
                return Some(cand);
            }
            return None;
        }
        i += 64;
    }

    // 3) Tail — any remaining bits below `max_bits` go through the per-bit path.
    while i < max_bits {
        if !bit_is_set(bitmap, i) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find the first run of `count` consecutive free bits at or after `start`,
/// within the first `max_bits` bits of the bitmap. Returns the starting bit
/// index of the run, or `None` if no such run exists.
///
/// Phase 8.3 vectorization: the outer "find a starting candidate" step
/// uses [`find_first_free`] (u64-stride skip over fully-used regions),
/// then the run-length verification walks bit-at-a-time. On sparse
/// bitmaps (typical post-mkfs) this is effectively memory-bandwidth
/// bound; on densely-packed bitmaps it skips fully-used 64-bit words in
/// one branch instead of 64.
pub fn find_free_run(bitmap: &[u8], start: u32, max_bits: u32, count: u32) -> Option<u32> {
    if count == 0 {
        return None;
    }
    let mut i = start;
    while i + count <= max_bits {
        // Vectorized: jump straight to the next free bit at-or-after `i`,
        // skipping all-ones words 64 bits at a time.
        let run_start = find_first_free(bitmap, i, max_bits)?;
        if run_start + count > max_bits {
            return None;
        }
        // Verify `count` contiguous free bits — bit-at-a-time, since the
        // blocker (if any) almost always sits within the first few bits.
        let mut j = run_start + 1;
        while j < run_start + count && !bit_is_set(bitmap, j) {
            j += 1;
        }
        if j - run_start >= count {
            return Some(run_start);
        }
        // Hit a used bit before reaching `count`; skip past it and retry.
        i = j + 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Block allocator (E5)
// ---------------------------------------------------------------------------

/// Plan allocation of `count` contiguous blocks.
///
/// `bitmap_reader` is called with a BGD's `bg_block_bitmap` and must return
/// the full bitmap block (block_size bytes). Groups are tried in order:
/// hint_group first, then wrapping forward. For groups flagged
/// `BLOCK_UNINIT`, the bitmap is treated as all-free without reading.
pub fn plan_block_allocation<F>(
    sb: &Superblock,
    groups: &[BlockGroupDescriptor],
    count: u32,
    hint_group: u32,
    mut bitmap_reader: F,
) -> Result<BlockAllocationPlan>
where
    F: FnMut(u64) -> Result<Vec<u8>>,
{
    if count == 0 {
        return Err(Error::Corrupt("plan_block_allocation: count == 0"));
    }
    let blocks_per_group = sb.blocks_per_group;
    let ngroups = groups.len() as u32;
    if ngroups == 0 {
        return Err(Error::Corrupt("no block groups"));
    }
    let hint = hint_group.min(ngroups.saturating_sub(1));

    for step in 0..ngroups {
        let gi = (hint + step) % ngroups;
        let bgd = &groups[gi as usize];
        if bgd.free_blocks_count < count {
            continue;
        }
        // Compute how many blocks are actually valid in this group (last group
        // may be short).
        let max_bits = blocks_in_group(sb, gi);

        let bitmap_bytes: Vec<u8> = if bgd.flags().contains(BgdFlags::BLOCK_UNINIT) {
            vec![0u8; sb.block_size() as usize]
        } else {
            bitmap_reader(bgd.block_bitmap)?
        };

        let Some(bit_start) = find_free_run(&bitmap_bytes, 0, max_bits, count) else {
            continue;
        };

        let group_first_block =
            (gi as u64) * (blocks_per_group as u64) + sb.first_data_block as u64;
        let first_block = group_first_block + bit_start as u64;

        return Ok(BlockAllocationPlan {
            first_block,
            count,
            bitmap: BitmapWrite {
                bitmap_block: bgd.block_bitmap,
                bit_start,
                count,
                set: true,
            },
            bgd: BgdCounterUpdate {
                group_idx: gi,
                free_blocks_delta: -(count as i32),
                free_inodes_delta: 0,
                used_dirs_delta: 0,
            },
            sb: SuperblockCounterUpdate {
                free_blocks_delta: -(count as i64),
                free_inodes_delta: 0,
            },
        });
    }

    Err(Error::Corrupt(
        "no group has a contiguous free run of this size",
    ))
}

/// Returns the number of blocks that actually exist in group `gi` (the last
/// group may be shorter than `blocks_per_group`).
fn blocks_in_group(sb: &Superblock, gi: u32) -> u32 {
    let ngroups = sb.block_group_count() as u32;
    if gi + 1 < ngroups {
        return sb.blocks_per_group;
    }
    // Saturating: `s_first_data_block` is not among the fields
    // `Superblock::parse` bounds, and a value above `blocks_count`
    // wrapped this subtraction -- which made the last group's bit
    // ceiling far larger than the group, so the allocator handed out
    // blocks outside the filesystem.
    let remainder =
        sb.blocks_count.saturating_sub(sb.first_data_block as u64) % sb.blocks_per_group as u64;
    if remainder == 0 {
        sb.blocks_per_group
    } else {
        remainder as u32
    }
}

// ---------------------------------------------------------------------------
// Inode allocator (E6)
// ---------------------------------------------------------------------------

/// Plan allocation of a single inode. For directories, uses a simplified
/// Orlov heuristic to spread dirs across groups; for regular files, prefers
/// the `hint_group` (typically the parent directory's group).
pub fn plan_inode_allocation<F>(
    sb: &Superblock,
    groups: &[BlockGroupDescriptor],
    is_dir: bool,
    hint_group: u32,
    mut bitmap_reader: F,
) -> Result<InodeAllocationPlan>
where
    F: FnMut(u64) -> Result<Vec<u8>>,
{
    let ngroups = groups.len() as u32;
    if ngroups == 0 {
        return Err(Error::Corrupt("no block groups"));
    }

    let start_group = if is_dir {
        orlov_select_group(groups, hint_group)
    } else {
        hint_group.min(ngroups.saturating_sub(1))
    };

    for step in 0..ngroups {
        let gi = (start_group + step) % ngroups;
        let bgd = &groups[gi as usize];
        if bgd.free_inodes_count == 0 {
            continue;
        }

        let max_bits = sb.inodes_per_group;
        let bitmap_bytes: Vec<u8> = if bgd.flags().contains(BgdFlags::INODE_UNINIT) {
            vec![0u8; sb.block_size() as usize]
        } else {
            bitmap_reader(bgd.inode_bitmap)?
        };

        // THE RESERVED INODES ARE NOT AVAILABLE.
        //
        // Inodes below `s_first_ino` are the filesystem's own: 2 is the
        // root directory, 8 the journal. On a filesystem mke2fs wrote
        // their bits are set, so scanning from zero skipped them by
        // accident -- but the bitmap is bytes off the disk, and one
        // with those bits clear (or a group 0 flagged INODE_UNINIT,
        // which is read as an all-zero bitmap without being read at
        // all) handed out inode 2. `apply_create` then writes the new
        // file's inode image over the root directory's.
        let floor = if gi == 0 {
            sb.first_inode.saturating_sub(1)
        } else {
            0
        };
        let Some(bit_start) = find_first_free(&bitmap_bytes, floor, max_bits) else {
            continue;
        };

        // Inode numbers are 1-based: first inode in group 0 is inode 1.
        // Checked, because `inodes_per_group` is a superblock field and
        // the product wrapped in release -- a group-2 allocation came
        // back as inode 2 and was written over the root inode while
        // group 2's counters were debited.
        let inode = u64::from(gi)
            .checked_mul(u64::from(sb.inodes_per_group))
            .and_then(|base| base.checked_add(u64::from(bit_start) + 1))
            .filter(|n| *n <= u64::from(sb.inodes_count))
            .ok_or(Error::Corrupt(
                "the group's inode range does not fit in the filesystem",
            ))? as u32;

        return Ok(InodeAllocationPlan {
            inode,
            is_dir,
            bitmap: BitmapWrite {
                bitmap_block: bgd.inode_bitmap,
                bit_start,
                count: 1,
                set: true,
            },
            bgd: BgdCounterUpdate {
                group_idx: gi,
                free_blocks_delta: 0,
                free_inodes_delta: -1,
                used_dirs_delta: if is_dir { 1 } else { 0 },
            },
            sb: SuperblockCounterUpdate {
                free_blocks_delta: 0,
                free_inodes_delta: -1,
            },
        });
    }

    Err(Error::Corrupt("no group has a free inode"))
}

/// Orlov group selection (simplified). Chooses the group among the ngroups
/// starting at `hint` that currently has the fewest directories AND at least
/// average free inodes. If no group is clearly "good", falls back to `hint`.
fn orlov_select_group(groups: &[BlockGroupDescriptor], hint: u32) -> u32 {
    let ngroups = groups.len() as u32;
    if ngroups == 0 {
        return 0;
    }
    let hint = hint.min(ngroups.saturating_sub(1));

    let total_free_inodes: u64 = groups.iter().map(|g| g.free_inodes_count as u64).sum();
    let total_used_dirs: u64 = groups.iter().map(|g| g.used_dirs_count as u64).sum();
    let avg_free_inodes = total_free_inodes / ngroups as u64;
    let avg_used_dirs = total_used_dirs / ngroups as u64;

    // Walk all groups from hint and pick the first that has more free inodes
    // than the average AND fewer used dirs than the average. Fall back to the
    // group with the most free inodes overall.
    let mut best: Option<u32> = None;
    let mut best_score: i64 = i64::MIN;
    for step in 0..ngroups {
        let gi = (hint + step) % ngroups;
        let g = &groups[gi as usize];
        let fi = g.free_inodes_count as i64;
        let ud = g.used_dirs_count as i64;
        // Score: bonus if above avg inodes and below avg dirs.
        let mut score = fi - ud;
        if fi >= avg_free_inodes as i64 {
            score += 1000;
        }
        if ud <= avg_used_dirs as i64 {
            score += 500;
        }
        if score > best_score && g.free_inodes_count > 0 {
            best_score = score;
            best = Some(gi);
        }
    }
    best.unwrap_or(hint)
}

// ---------------------------------------------------------------------------
// Plan application helpers — pure functions that mutate caller-owned buffers.
// The actual disk writes happen in E11 (journaled writes).
// ---------------------------------------------------------------------------

/// Apply a [`BitmapWrite`] to a bitmap buffer in place.
pub fn apply_bitmap_write(buf: &mut [u8], w: &BitmapWrite) {
    for b in 0..w.count {
        let idx = (w.bit_start + b) as usize;
        let byte = idx / 8;
        let mask = 1u8 << (idx % 8);
        if byte >= buf.len() {
            break;
        }
        if w.set {
            buf[byte] |= mask;
        } else {
            buf[byte] &= !mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_sb(
        block_size: u32,
        blocks_per_group: u32,
        inodes_per_group: u32,
        total_blocks: u64,
    ) -> Superblock {
        // Minimal superblock constructed from a synthetic buffer.
        let mut raw = vec![0u8; crate::superblock::SUPERBLOCK_SIZE];
        // magic
        raw[0x38..0x3A].copy_from_slice(&(crate::superblock::EXT4_MAGIC).to_le_bytes());
        raw[0x00..0x04].copy_from_slice(&(inodes_per_group * 4).to_le_bytes());
        raw[0x04..0x08].copy_from_slice(&(total_blocks as u32).to_le_bytes());
        raw[0x14..0x18].copy_from_slice(&1u32.to_le_bytes()); // first_data_block
        raw[0x18..0x1C]
            .copy_from_slice(&(block_size.trailing_zeros().saturating_sub(10)).to_le_bytes());
        raw[0x20..0x24].copy_from_slice(&blocks_per_group.to_le_bytes());
        raw[0x28..0x2C].copy_from_slice(&inodes_per_group.to_le_bytes());
        raw[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes()); // rev_level=1 so inode_size read
        raw[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes()); // inode_size
        raw[0xFE..0x100].copy_from_slice(&64u16.to_le_bytes()); // desc_size
        crate::superblock::Superblock::parse(raw).unwrap()
    }

    fn mk_bgd(
        free_blocks: u32,
        free_inodes: u32,
        used_dirs: u32,
        flags: u16,
    ) -> BlockGroupDescriptor {
        BlockGroupDescriptor {
            block_bitmap: 100,
            inode_bitmap: 200,
            inode_table: 300,
            free_blocks_count: free_blocks,
            free_inodes_count: free_inodes,
            used_dirs_count: used_dirs,
            flags,
            itable_unused: 0,
            block_bitmap_csum: 0,
            inode_bitmap_csum: 0,
            checksum: 0,
        }
    }

    /// `bit_is_set` answers "not set" for an index past the end of the
    /// buffer, which reads as free -- and `max_bits` comes from
    /// `s_blocks_per_group` / `s_inodes_per_group`, superblock fields
    /// that nothing bounds to what a bitmap block can hold.
    ///
    /// The plan then names a real inode or block while the write that
    /// marks it used silently does nothing (the byte is past the
    /// buffer), and the counters are debited anyway. The next
    /// allocation returns the same one: every file on the same block,
    /// every new inode over the last.
    #[test]
    fn a_bit_outside_the_bitmap_is_not_a_free_bit() {
        // One byte of bitmap, every bit used, but a group claiming
        // 2^31 bits.
        let full = vec![0xFFu8];
        assert_eq!(find_first_free(&full, 0, 1 << 31), None);
        // And with room in the byte, only the bits that are there.
        let partly = vec![0x0Fu8];
        assert_eq!(find_first_free(&partly, 0, 1 << 31), Some(4));
        assert_eq!(find_first_free(&partly, 8, 1 << 31), None);
        // An empty bitmap has no free bits, however many are claimed.
        assert_eq!(find_first_free(&[], 0, 1 << 31), None);
    }

    /// Inodes below `s_first_ino` are the filesystem's own: 2 is the
    /// root directory, 8 the journal. On a filesystem mke2fs wrote,
    /// their bits are set, so scanning from zero skipped them by
    /// accident -- but the bitmap is bytes off the disk, and a group 0
    /// flagged INODE_UNINIT is read as an all-zero bitmap without being
    /// read at all.
    #[test]
    fn a_reserved_inode_is_never_allocated() {
        let sb = mk_sb(1024, 8192, 128, 1024);
        // Group 0 declared uninitialised, so its bitmap is taken as
        // all-free without a device read.
        let groups = vec![mk_bgd(100, 128, 0, BgdFlags::INODE_UNINIT.bits())];
        assert_eq!(
            sb.first_inode,
            crate::superblock::GOOD_OLD_FIRST_INODE,
            "the fixture superblock leaves s_first_ino zero; the floor comes from the parser"
        );
        let plan = plan_inode_allocation(&sb, &groups, false, 0, |_| {
            panic!("an uninitialised group is not read")
        })
        .expect("a group with free inodes");
        assert!(
            plan.inode >= sb.first_inode,
            "allocated inode {} , where the first non-reserved one is {} -- \
             writing it puts the new file's inode over the root directory's",
            plan.inode,
            sb.first_inode
        );

        // Group 1 has no reserved inodes, so it starts at its first bit.
        let groups = vec![
            mk_bgd(100, 0, 0, 0),
            mk_bgd(100, 128, 0, BgdFlags::INODE_UNINIT.bits()),
        ];
        let plan = plan_inode_allocation(&sb, &groups, false, 1, |_| unreachable!()).unwrap();
        assert_eq!(plan.inode, 128 + 1);
    }

    #[test]
    fn find_first_free_walks_bits() {
        let buf = vec![0xFF, 0x0F, 0x00]; // bits 0..11 set, 12..23 free
        assert_eq!(find_first_free(&buf, 0, 24), Some(12));
        assert_eq!(find_first_free(&buf, 20, 24), Some(20));
    }

    #[test]
    fn find_first_free_word_aligned_fast_path() {
        // 16 bytes = 128 bits. First 64 bits all set; bit 80 is the first free.
        let mut buf = vec![0xFFu8; 8];
        buf.extend_from_slice(&[0xFFu8; 2]); // bits 64..79 set
        buf.push(0x00); // bit 80..87 free → first free is 80
        buf.extend_from_slice(&[0xFFu8; 5]);
        assert_eq!(find_first_free(&buf, 0, 128), Some(80));
    }

    #[test]
    fn find_first_free_all_ones_in_range() {
        // Whole range is allocated; must return None without overflow.
        let buf = vec![0xFFu8; 32]; // 256 bits, all set
        assert_eq!(find_first_free(&buf, 0, 256), None);
        assert_eq!(find_first_free(&buf, 63, 256), None);
        assert_eq!(find_first_free(&buf, 64, 256), None);
    }

    #[test]
    fn find_first_free_respects_max_bits_mid_word() {
        // Two zero bits starting at 64; max_bits caps at 65 → bit 64 valid, 65 out.
        let mut buf = vec![0xFFu8; 8]; // bits 0..63 set
        buf.push(0x00); // bits 64..71 free
        buf.extend_from_slice(&[0xFFu8; 7]);
        assert_eq!(find_first_free(&buf, 0, 65), Some(64));
        assert_eq!(find_first_free(&buf, 0, 64), None);
    }

    #[test]
    fn find_first_free_unaligned_start_matches_per_bit() {
        // Reference: every result must agree with the simple per-bit implementation.
        let buf: Vec<u8> = (0..128u8).collect(); // mixed pattern
        let max = (buf.len() as u32) * 8;
        for start in [0u32, 1, 7, 8, 63, 64, 65, 127, 200, 511] {
            let fast = find_first_free(&buf, start, max);
            let slow = {
                let mut i = start;
                loop {
                    if i >= max {
                        break None;
                    }
                    if !bit_is_set(&buf, i) {
                        break Some(i);
                    }
                    i += 1;
                }
            };
            assert_eq!(fast, slow, "start={start}");
        }
    }

    #[test]
    fn find_free_run_handles_gaps() {
        // byte 0: bits 0,1 set; bits 2..=7 free. Byte 1: all set. Byte 2: all free.
        let buf = vec![0b0000_0011, 0xFF, 0x00];
        // Shortest run: bits 2..=7 = run of 6. A request for 5 fits at bit 2.
        assert_eq!(find_free_run(&buf, 0, 24, 5), Some(2));
        // A request for 7 cannot fit in bits 2..7 (only 6 free) — next run is byte 2.
        assert_eq!(find_free_run(&buf, 0, 24, 7), Some(16));
    }

    #[test]
    fn find_free_run_exact_fit() {
        let buf = vec![0x00];
        assert_eq!(find_free_run(&buf, 0, 8, 8), Some(0));
    }

    #[test]
    fn find_free_run_rejects_too_short() {
        let buf = vec![0xFE]; // bit 0 free, bits 1..7 used
        assert_eq!(find_free_run(&buf, 0, 8, 2), None);
    }

    #[test]
    fn block_allocation_uses_first_group_with_room() {
        let sb = mk_sb(4096, 32768, 8192, 65536);
        let g0 = mk_bgd(100, 8000, 0, 0);
        let g1 = mk_bgd(20000, 8000, 0, 0);
        let groups = vec![g0, g1];
        let read = |_block: u64| -> Result<Vec<u8>> { Ok(vec![0u8; 4096]) };
        let plan = plan_block_allocation(&sb, &groups, 10, 0, read).unwrap();
        assert_eq!(plan.first_block, 1); // first_data_block=1, group 0 bit 0
        assert_eq!(plan.count, 10);
        assert_eq!(plan.bgd.group_idx, 0);
        assert_eq!(plan.bgd.free_blocks_delta, -10);
        assert_eq!(plan.sb.free_blocks_delta, -10);
    }

    #[test]
    fn block_allocation_skips_full_group() {
        let sb = mk_sb(4096, 32768, 8192, 65536);
        let g0 = mk_bgd(5, 8000, 0, 0); // not enough for 10-block run
        let g1 = mk_bgd(20000, 8000, 0, 0);
        let groups = vec![g0, g1];
        let read = |_b| Ok(vec![0u8; 4096]);
        let plan = plan_block_allocation(&sb, &groups, 10, 0, read).unwrap();
        assert_eq!(plan.bgd.group_idx, 1);
        // Block 1 + group1_offset
        assert_eq!(plan.first_block, 1 + 32768);
    }

    #[test]
    fn block_allocation_honours_block_uninit_flag() {
        let sb = mk_sb(4096, 32768, 8192, 65536);
        // group 0 is UNINIT → treated as all-free without reading bitmap
        let g0 = mk_bgd(32768, 8000, 0, BgdFlags::BLOCK_UNINIT.bits());
        let groups = vec![g0];
        let mut call_count = 0;
        let read = |_b| {
            call_count += 1;
            Ok(vec![0xFFu8; 4096])
        };
        let plan = plan_block_allocation(&sb, &groups, 4, 0, read).unwrap();
        assert_eq!(plan.count, 4);
        assert_eq!(call_count, 0, "UNINIT group should not read bitmap");
    }

    /// Inode numbers are one-based, and group 0's first *available*
    /// one is `s_first_ino`.
    ///
    /// This used to assert inode 1, on an all-free bitmap that no real
    /// filesystem has: mke2fs sets the reserved bits, so scanning from
    /// zero returned 11 anyway and the assertion only held for the
    /// synthetic bitmap here. It is the reserved range the allocator
    /// must not enter, not the bits that happen to be set.
    #[test]
    fn inode_allocation_returns_one_based_number() {
        let sb = mk_sb(4096, 32768, 8192, 65536);
        let g0 = mk_bgd(1000, 8000, 0, 0);
        let groups = vec![g0];
        let read = |_b| Ok(vec![0u8; 4096]);
        let plan = plan_inode_allocation(&sb, &groups, false, 0, read).unwrap();
        assert_eq!(
            plan.inode, sb.first_inode,
            "group 0's first available inode is s_first_ino, not inode 1"
        );
        assert_eq!(plan.bitmap.bit_start, sb.first_inode - 1, "one-based");
        assert!(!plan.is_dir);
        assert_eq!(plan.bgd.used_dirs_delta, 0);
    }

    #[test]
    fn inode_allocation_dir_bumps_used_dirs() {
        let sb = mk_sb(4096, 32768, 8192, 65536);
        let g0 = mk_bgd(1000, 8000, 0, 0);
        let groups = vec![g0];
        let read = |_b| Ok(vec![0u8; 4096]);
        let plan = plan_inode_allocation(&sb, &groups, true, 0, read).unwrap();
        assert!(plan.is_dir);
        assert_eq!(plan.bgd.used_dirs_delta, 1);
        assert_eq!(plan.bgd.free_inodes_delta, -1);
    }

    #[test]
    fn orlov_prefers_group_with_fewer_dirs() {
        // g1 has fewer dirs and more free inodes — should win the Orlov beauty contest.
        let groups = vec![
            mk_bgd(100, 500, 30, 0),
            mk_bgd(100, 1000, 2, 0),
            mk_bgd(100, 800, 10, 0),
        ];
        assert_eq!(orlov_select_group(&groups, 0), 1);
    }

    #[test]
    fn apply_bitmap_write_sets_and_clears_bits() {
        let mut buf = vec![0u8; 2];
        apply_bitmap_write(
            &mut buf,
            &BitmapWrite {
                bitmap_block: 0,
                bit_start: 0,
                count: 10,
                set: true,
            },
        );
        assert_eq!(buf, vec![0xFF, 0x03]);
        apply_bitmap_write(
            &mut buf,
            &BitmapWrite {
                bitmap_block: 0,
                bit_start: 5,
                count: 3,
                set: false,
            },
        );
        assert_eq!(buf, vec![0b0001_1111, 0x03]);
    }

    // --- blocks_in_group ---

    #[test]
    fn blocks_in_group_full_groups_return_blocks_per_group() {
        // 3 full groups of 32768 each: total = 3*32768 + 1 (first_data_block=1).
        let sb = mk_sb(4096, 32768, 8192, 3 * 32768 + 1);
        // Groups 0 and 1 are not the last group, so they return blocks_per_group.
        assert_eq!(blocks_in_group(&sb, 0), 32768);
        assert_eq!(blocks_in_group(&sb, 1), 32768);
    }

    #[test]
    fn blocks_in_group_last_group_exact_multiple_returns_full() {
        // usable = 2*32768, first_data_block=1 → blocks_count = 2*32768+1
        // remainder = (2*32768+1-1) % 32768 = 65536 % 32768 = 0 → full group
        let sb = mk_sb(4096, 32768, 8192, 2 * 32768 + 1);
        assert_eq!(blocks_in_group(&sb, 1), 32768);
    }

    #[test]
    fn blocks_in_group_short_last_group() {
        // 1 full group + 100 extra blocks: total = 32768 + 100 + 1 = 32869
        let sb = mk_sb(4096, 32768, 8192, 32769 + 100);
        assert_eq!(blocks_in_group(&sb, 0), 32768); // full
        assert_eq!(blocks_in_group(&sb, 1), 100); // short last group
    }
}
