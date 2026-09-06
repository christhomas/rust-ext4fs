//! Inline data reading.
//!
//! Spec: kernel.org/doc/html/latest/filesystems/ext4/inlinedata.html
//!
//! When the `INCOMPAT_INLINE_DATA` feature is enabled and an inode has the
//! `EXT4_INLINE_DATA_FL` flag, the file's contents live inside the inode
//! itself instead of being stored in extent-allocated data blocks.
//!
//! Layout:
//!   - First **60 bytes** of the file are stored in the `i_block[60]` array
//!     (the same field that normally holds the extent header / direct block
//!     pointers).
//!   - If the file is larger than 60 bytes, the remainder is stored as the
//!     value of a special xattr named `system.data`. Concatenate the two to
//!     get the full file content.
//!   - Maximum inline file size = 60 + (in-inode-xattr-region-size minus
//!     headers and other entries). Typically 60–~150 bytes for inode_size=256.

use crate::block_io::BlockDevice;
use crate::error::Result;
use crate::inode::Inode;
use crate::xattr;

/// Read the contents of an inline-data file in full.
///
/// Returns the concatenation of:
/// 1. `inode.block` (60 bytes), truncated to the file's `size`
/// 2. The `system.data` xattr value (if file size > 60)
///
/// Caller must verify the inode actually has `INLINE_DATA_FL` set before
/// calling — otherwise the returned bytes are garbage (extent header etc.).
pub fn read_all(
    dev: &dyn BlockDevice,
    inode: &Inode,
    inode_raw: &[u8],
    inode_size: u16,
    block_size: u32,
) -> Result<Vec<u8>> {
    let total = inode.size as usize;

    // Up to 60 bytes from i_block.
    let inline_max = 60;
    let from_block = total.min(inline_max);
    // Reserved for what inline data can actually be -- the 60 bytes of
    // i_block plus the in-inode xattr region, which is bounded by the
    // inode size -- rather than for whatever `i_size` claimed. The
    // vector grows to what is really there.
    let mut out = Vec::with_capacity(total.min(64 * 1024));
    out.extend_from_slice(&inode.block[..from_block]);

    if total <= inline_max {
        return Ok(out);
    }

    // Overflow lives in the system.data xattr.
    //
    // A missing or short xattr is CORRUPTION, not an empty tail. The
    // inode's size field says the file is `total` bytes; if the bytes
    // are not there, returning the 60 we do have would hand the caller
    // a silently truncated file that still reports its full length —
    // the worst of both, since nothing downstream can tell.
    let need = total - inline_max;
    let extra = xattr::get(dev, inode, inode_raw, inode_size, block_size, "system.data")?.ok_or(
        crate::error::Error::Corrupt("inline file larger than 60 bytes has no system.data xattr"),
    )?;
    if extra.len() < need {
        return Err(crate::error::Error::Corrupt(
            "inline file's system.data xattr is shorter than its size claims",
        ));
    }
    out.extend_from_slice(&extra[..need]);

    Ok(out)
}

/// Read a range from an inline-data file.
/// Returns the bytes copied into `dst`, or `Ok(0)` if `offset >= size`.
pub fn read_range(
    dev: &dyn BlockDevice,
    inode: &Inode,
    inode_raw: &[u8],
    inode_size: u16,
    block_size: u32,
    offset: u64,
    dst: &mut [u8],
) -> Result<usize> {
    let total = inode.size;
    if offset >= total {
        return Ok(0);
    }
    let full = read_all(dev, inode, inode_raw, inode_size, block_size)?;
    let want = ((total - offset) as usize).min(dst.len());
    let avail = full.len().saturating_sub(offset as usize);
    let n = want.min(avail);
    dst[..n].copy_from_slice(&full[offset as usize..offset as usize + n]);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inode::{Inode, OFF_MODE, OFF_SIZE_LO};

    /// A device with nothing on it. The inline path only reaches the
    /// device to look for the `system.data` xattr, and these tests are
    /// about what happens when that xattr is not there.
    struct EmptyDev;

    impl BlockDevice for EmptyDev {
        fn read_at(&self, _off: u64, buf: &mut [u8]) -> Result<()> {
            buf.fill(0);
            Ok(())
        }
        fn write_at(&self, _off: u64, _buf: &[u8]) -> Result<()> {
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            1 << 20
        }
    }

    /// A 128-byte inode claiming `size` bytes of a regular file, with
    /// `i_block` filled with a recognisable pattern.
    fn inline_inode(size: u32) -> (Inode, Vec<u8>) {
        let mut raw = vec![0u8; 128];
        raw[OFF_MODE..OFF_MODE + 2].copy_from_slice(&0o100_644u16.to_le_bytes());
        raw[OFF_SIZE_LO..OFF_SIZE_LO + 4].copy_from_slice(&size.to_le_bytes());
        // i_block at 0x28, 60 bytes.
        for (i, b) in raw[0x28..0x28 + 60].iter_mut().enumerate() {
            *b = b'A'.wrapping_add((i % 26) as u8);
        }
        let inode = Inode::parse(&raw).expect("parse synthetic inode");
        (inode, raw)
    }

    /// A file that fits entirely in `i_block` needs no xattr, so an
    /// empty device is fine.
    #[test]
    fn a_file_within_the_inline_area_reads_without_an_xattr() {
        let (inode, raw) = inline_inode(60);
        let got =
            read_all(&EmptyDev, &inode, &raw, 128, 4096).expect("60 bytes are all in i_block");
        assert_eq!(got.len(), 60);
    }

    /// **The fix.** A file claiming more than 60 bytes whose
    /// `system.data` xattr is absent is corrupt, and must say so.
    ///
    /// Before this, the missing xattr was skipped silently and the
    /// caller received 60 bytes for a file whose size field said more
    /// — a truncated read that nothing downstream could detect,
    /// because the length in the metadata still claimed the full size.
    #[test]
    fn a_missing_spill_xattr_is_corruption_not_an_empty_tail() {
        let (inode, raw) = inline_inode(100);
        let err = read_all(&EmptyDev, &inode, &raw, 128, 4096)
            .expect_err("100 bytes cannot fit in the 60-byte inline area without a spill");
        assert!(
            format!("{err:?}").contains("system.data"),
            "the error should name the missing xattr, got: {err:?}"
        );
    }
}
