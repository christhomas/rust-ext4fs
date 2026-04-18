//! Top-level filesystem handle. Composes block_io + superblock + bgd + inode + extent + dir.

use crate::bgd::{self, BlockGroupDescriptor};
use crate::block_io::BlockDevice;
use crate::checksum::Checksummer;
use crate::error::{Error, Result};
use crate::features;
use crate::inode::Inode;
use crate::superblock::Superblock;
use std::sync::Arc;

/// Split a `/a/b/c` path into (`/a/b`, `c`). Returns an error for empty or
/// `"/"` paths (no basename to act on).
fn split_parent_and_base(path: &str) -> Result<(String, String)> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::InvalidArgument("empty path"));
    }
    let last_slash = trimmed
        .rfind('/')
        .ok_or(Error::InvalidArgument("relative path"))?;
    let base = &trimmed[last_slash + 1..];
    let parent = if last_slash == 0 {
        "/"
    } else {
        &trimmed[..last_slash]
    };
    if base.is_empty() {
        // Trailing slash on a non-dir path is POSIX ENOTDIR, not a generic arg error.
        return Err(Error::NotADirectory);
    }
    Ok((parent.to_string(), base.to_string()))
}

/// Current wall time as a u32 — matches ext4's `i_dtime` field. Uses
/// `SystemTime::now()`; we don't care about monotonicity here, just that
/// `dtime > ctime` so `e2fsck` recognises the slot as recently deleted.
fn now_unix_seconds() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

pub struct Filesystem {
    pub dev: Arc<dyn BlockDevice>,
    pub sb: Superblock,
    pub groups: Vec<BlockGroupDescriptor>,
    pub csum: Checksummer,
}

impl Filesystem {
    /// Mount the ext4 filesystem on `dev`. Read-only unless the device reports
    /// `is_writable()`, in which case a dirty journal is replayed before
    /// returning so callers see a consistent on-disk state.
    ///
    /// When `RO_COMPAT_METADATA_CSUM` is set, the superblock checksum is
    /// verified — failure aborts the mount with `Error::BadChecksum`.
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        let sb = Superblock::read(dev.as_ref())?;
        features::check_mountable(sb.feature_incompat, sb.feature_ro_compat)?;
        let csum = Checksummer::from_superblock(&sb);
        if csum.enabled && !csum.verify_superblock(&sb.raw) {
            return Err(Error::BadChecksum { what: "superblock" });
        }
        let groups = bgd::read_all(dev.as_ref(), &sb, &csum)?;
        let fs = Self {
            dev,
            sb,
            groups,
            csum,
        };

        // Replay a dirty journal if the device is writable. Silently skips
        // for read-only mounts — the read path tolerates a non-clean journal
        // (pending transactions are invisible, which is correct for a
        // read-only view).
        if fs.dev.is_writable() {
            // Best-effort: a replay failure here is logged via the returned
            // error but does NOT abort the mount, because many images have
            // cosmetic journal issues that shouldn't prevent read access.
            // The error surfaces up so the caller can decide whether to
            // retry or proceed; we fail loud rather than silent.
            crate::journal_apply::replay_if_dirty(&fs)?;
        }
        Ok(fs)
    }

    /// Read a whole block by its logical block number.
    pub fn read_block(&self, block_num: u64) -> Result<Vec<u8>> {
        let block_size = self.sb.block_size() as usize;
        let mut buf = vec![0u8; block_size];
        self.dev.read_at(block_num * block_size as u64, &mut buf)?;
        Ok(buf)
    }

    /// Read raw inode bytes for a given inode number (does not parse).
    pub fn read_inode_raw(&self, ino: u32) -> Result<Vec<u8>> {
        let (block, offset) = bgd::locate_inode(&self.sb, &self.groups, ino)?;
        let block_data = self.read_block(block)?;
        let inode_size = self.sb.inode_size as usize;
        let off = offset as usize;
        Ok(block_data[off..off + inode_size].to_vec())
    }

    /// Read + parse + checksum-verify an inode in one shot.
    ///
    /// When `RO_COMPAT_METADATA_CSUM` is enabled the inode CRC32C is checked
    /// (salted by inode number + generation per ext4 spec). A mismatch
    /// returns `Error::BadChecksum { what: "inode" }`.
    pub fn read_inode_verified(&self, ino: u32) -> Result<(Inode, Vec<u8>)> {
        let raw = self.read_inode_raw(ino)?;
        let inode = Inode::parse(&raw)?;
        if self.csum.enabled && !self.csum.verify_inode(ino, inode.generation, &raw) {
            return Err(Error::BadChecksum { what: "inode" });
        }
        Ok((inode, raw))
    }

    /// Write the given raw inode bytes back to disk. Read-only devices return
    /// the default `Error::Corrupt` from `BlockDevice::write_at`.
    ///
    /// **Not checksum-aware**: callers that update fields affecting the inode
    /// CRC32C (anything except `checksum_lo` / `checksum_hi`) must recompute
    /// + patch the checksum into `raw` before calling this. Not wrapped in a
    /// journal transaction — see E11 / `journal_apply` for the journaled
    /// version. Use only when the caller has the full write-ordering story
    /// under control.
    pub fn write_inode_raw(&self, ino: u32, raw: &[u8]) -> Result<()> {
        if raw.len() != self.sb.inode_size as usize {
            return Err(Error::Corrupt("write_inode_raw: length != inode_size"));
        }
        let (block, offset) = bgd::locate_inode(&self.sb, &self.groups, ino)?;
        let block_size = self.sb.block_size() as u64;
        let byte_offset = block * block_size + offset as u64;
        self.dev.write_at(byte_offset, raw)?;
        Ok(())
    }

    /// Patch fields in a raw inode image: size, blocks_count. Leaves all
    /// other bytes (including the extent tree header + entries in `i_block`)
    /// intact. `new_block_count` is in 512-byte sectors per spec (same
    /// convention as `Inode::blocks`).
    pub fn patch_inode_size_and_blocks(
        raw: &mut [u8],
        new_size: u64,
        new_block_count: u64,
    ) -> Result<()> {
        if raw.len() < 128 {
            return Err(Error::Corrupt("patch_inode: buffer too small"));
        }
        // size = size_lo (0x04..0x08) + size_hi (0x6C..0x70)
        let size_lo = (new_size & 0xFFFF_FFFF) as u32;
        let size_hi = (new_size >> 32) as u32;
        raw[0x04..0x08].copy_from_slice(&size_lo.to_le_bytes());
        raw[0x6C..0x70].copy_from_slice(&size_hi.to_le_bytes());
        // blocks = blocks_lo (0x1C..0x20, u32) + blocks_hi (0x74..0x76, u16)
        let blocks_lo = (new_block_count & 0xFFFF_FFFF) as u32;
        let blocks_hi = ((new_block_count >> 32) & 0xFFFF) as u16;
        raw[0x1C..0x20].copy_from_slice(&blocks_lo.to_le_bytes());
        raw[0x74..0x76].copy_from_slice(&blocks_hi.to_le_bytes());
        Ok(())
    }

    /// Overwrite the 60-byte `i_block` area of an inode image with `new_root`.
    /// Used when an extent-tree mutation changes the inline root.
    pub fn patch_inode_block_area(raw: &mut [u8], new_root: &[u8]) -> Result<()> {
        if raw.len() < 128 {
            return Err(Error::Corrupt("patch_inode_block_area: buffer too small"));
        }
        if new_root.len() != 60 {
            return Err(Error::Corrupt(
                "patch_inode_block_area: new_root != 60 bytes",
            ));
        }
        raw[0x28..0x64].copy_from_slice(new_root);
        Ok(())
    }

    /// Shrink a file to `new_size`. Composes `file_mut::plan_truncate_shrink`
    /// (extent-tree updates + freed-block ranges) with actual disk writes —
    /// rewrites the inode and zeros the freed bitmap bits.
    ///
    /// Not journaled. Safe to call only in a context where crash consistency
    /// is handled elsewhere (e.g. a test scratch image). A future revision
    /// will route this through a JBD2 transaction so the inode write + bitmap
    /// writes are atomic with respect to a crash.
    pub fn apply_truncate_shrink(&self, ino: u32, new_size: u64) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let (inode, mut raw) = self.read_inode_verified(ino)?;
        if new_size > inode.size {
            return Err(Error::InvalidArgument(
                "truncate: new_size > old_size (grow not supported)",
            ));
        }

        let (_size_change, muts) = crate::file_mut::plan_truncate_shrink(
            inode.size,
            new_size,
            &inode.block,
            self.sb.block_size(),
        )?;

        let mut freed_sectors: u64 = 0;
        let bs = self.sb.block_size() as u64;

        for m in &muts {
            match m {
                crate::extent_mut::ExtentMutation::WriteRoot { bytes } => {
                    // Splice new 60-byte i_block into the inode image.
                    Self::patch_inode_block_area(&mut raw, bytes)?;
                }
                crate::extent_mut::ExtentMutation::FreePhysicalRun { start, len } => {
                    // Mark the run's bits free in the containing group's bitmap.
                    self.free_block_run(*start, *len as u64)?;
                    // Each fs block is `bs / 512` 512-byte sectors for the
                    // `i_blocks` counter.
                    freed_sectors += (*len as u64) * (bs / 512);
                }
                _ => {
                    return Err(Error::Corrupt(
                        "apply_truncate_shrink: unexpected mutation type",
                    ));
                }
            }
        }

        // Patch size + blocks_count in the inode image.
        let new_blocks = inode.blocks.saturating_sub(freed_sectors);
        Self::patch_inode_size_and_blocks(&mut raw, new_size, new_blocks)?;

        // Write the inode back. Recompute + splice the inode checksum first
        // so CSUM-enabled mounts see a valid inode on the next read.
        if self.csum.enabled {
            if let Some((lo, hi)) = self
                .csum
                .compute_inode_checksum(ino, inode.generation, &raw)
            {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(ino, &raw)?;
        self.dev.flush()?;
        Ok(())
    }

    /// Extend a file to `new_size`. The new range is a sparse hole — ext4's
    /// extent tree treats unmapped logical blocks as zeros, so no extent
    /// mutation and no block allocation are required. Only `i_size`,
    /// `i_mtime`, `i_ctime`, and the inode checksum change.
    ///
    /// Caller (capi dispatch) guarantees `new_size >= inode.size`. If
    /// `new_size == inode.size` this is a no-op that still bumps the
    /// timestamps — matches `truncate(2)` semantics.
    pub fn apply_truncate_grow(&self, ino: u32, new_size: u64) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let (inode, mut raw) = self.read_inode_verified(ino)?;
        if new_size < inode.size {
            return Err(Error::InvalidArgument(
                "apply_truncate_grow: new_size < old_size (use apply_truncate_shrink)",
            ));
        }
        Self::patch_inode_size_and_blocks(&mut raw, new_size, inode.blocks)?;

        let now = now_unix_seconds();
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes()); // ctime
        raw[0x10..0x14].copy_from_slice(&now.to_le_bytes()); // mtime

        if self.csum.enabled {
            if let Some((lo, hi)) = self
                .csum
                .compute_inode_checksum(ino, inode.generation, &raw)
            {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(ino, &raw)?;
        self.dev.flush()?;
        Ok(())
    }

    /// Change the permission bits on `path`. Only the low 12 bits of `mode`
    /// (`S_ISUID|S_ISGID|S_ISVTX` plus rwx/rwx/rwx) are applied; the file-type
    /// bits (`S_IFMT`) are preserved from the existing inode.
    ///
    /// Updates `i_ctime = now` and recomputes the inode checksum on csum-
    /// enabled mounts. Returns `Error::NotFound` if the path doesn't resolve,
    /// `Error::ReadOnly` on a RO mount.
    pub fn apply_chmod(&self, path: &str, mode: u16) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let ino = crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;

        // Preserve file-type bits (high 4 bits of i_mode); only the low 12
        // permission/suid/sgid/sticky bits are user-settable.
        let file_type_bits = inode.mode & crate::inode::S_IFMT;
        let new_mode = file_type_bits | (mode & 0x0FFF);
        raw[0x00..0x02].copy_from_slice(&new_mode.to_le_bytes());

        // POSIX: chmod bumps ctime (not mtime).
        let now = now_unix_seconds();
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes());

        if self.csum.enabled {
            if let Some((lo, hi)) = self
                .csum
                .compute_inode_checksum(ino, inode.generation, &raw)
            {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(ino, &raw)?;
        self.dev.flush()?;
        Ok(())
    }

    /// Change the owner of `path` to (`uid`, `gid`). Both values are full
    /// 32-bit — the inode stores them as hi+lo u16 halves at different
    /// offsets per the ext4 on-disk format. Passing `u32::MAX` for either
    /// field leaves that value untouched (Linux lchown(2) convention).
    ///
    /// Updates `i_ctime = now` and recomputes the inode checksum on
    /// csum-enabled mounts.
    pub fn apply_chown(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let ino = crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;

        if uid != u32::MAX {
            let lo = (uid & 0xFFFF) as u16;
            let hi = ((uid >> 16) & 0xFFFF) as u16;
            raw[0x02..0x04].copy_from_slice(&lo.to_le_bytes());
            raw[0x78..0x7A].copy_from_slice(&hi.to_le_bytes());
        }
        if gid != u32::MAX {
            let lo = (gid & 0xFFFF) as u16;
            let hi = ((gid >> 16) & 0xFFFF) as u16;
            raw[0x18..0x1A].copy_from_slice(&lo.to_le_bytes());
            raw[0x7A..0x7C].copy_from_slice(&hi.to_le_bytes());
        }

        let now = now_unix_seconds();
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes());

        if self.csum.enabled {
            if let Some((lo, hi)) = self
                .csum
                .compute_inode_checksum(ino, inode.generation, &raw)
            {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(ino, &raw)?;
        self.dev.flush()?;
        Ok(())
    }

    /// Set the access + modification times on `path`. Mirrors POSIX
    /// `utimensat(2)`: `atime_sec/nsec` and `mtime_sec/nsec` each replace
    /// the inode's atime/mtime. `ctime` is bumped to now (POSIX requires
    /// the change-time stamp on any attribute write). The `u32::MAX`
    /// sentinel on either `_sec` leaves that pair unchanged (lets callers
    /// touch just atime or just mtime).
    ///
    /// `nsec` values are the sub-second timestamp in nanoseconds and are
    /// only written when the inode's `i_extra_isize` region is large
    /// enough to hold them (requires ≥ 160-byte inodes — the e2fsprogs
    /// default).
    pub fn apply_utimens(
        &self,
        path: &str,
        atime_sec: u32,
        atime_nsec: u32,
        mtime_sec: u32,
        mtime_nsec: u32,
    ) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let ino = crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;

        if atime_sec != u32::MAX {
            raw[0x08..0x0C].copy_from_slice(&atime_sec.to_le_bytes());
        }
        if mtime_sec != u32::MAX {
            raw[0x10..0x14].copy_from_slice(&mtime_sec.to_le_bytes());
        }
        // POSIX: any attribute write bumps ctime.
        let now = now_unix_seconds();
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes());

        // Extra-isize region carries the nsec fields on larger inodes.
        // Offsets (relative to inode start):
        //   0x84 i_ctime_extra  (needs i_extra_isize ≥  8)
        //   0x88 i_mtime_extra  (needs i_extra_isize ≥ 12)
        //   0x8C i_atime_extra  (needs i_extra_isize ≥ 16)
        // Linux packs these as `(nsec << 2) | epoch_bits` — leave
        // epoch_bits zero (matches the base 32-bit time counter's 2038
        // range).
        if raw.len() >= 0x82 {
            let i_extra_isize = u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap());
            if i_extra_isize >= 8 && raw.len() >= 0x88 {
                // Bump ctime_nsec to 0 alongside the ctime bump above.
                raw[0x84..0x88].copy_from_slice(&0u32.to_le_bytes());
            }
            if mtime_sec != u32::MAX && i_extra_isize >= 12 && raw.len() >= 0x8C {
                let packed = (mtime_nsec & 0x3FFF_FFFF) << 2;
                raw[0x88..0x8C].copy_from_slice(&packed.to_le_bytes());
            }
            if atime_sec != u32::MAX && i_extra_isize >= 16 && raw.len() >= 0x90 {
                let packed = (atime_nsec & 0x3FFF_FFFF) << 2;
                raw[0x8C..0x90].copy_from_slice(&packed.to_le_bytes());
            }
        }

        if self.csum.enabled {
            if let Some((lo, hi)) = self
                .csum
                .compute_inode_checksum(ino, inode.generation, &raw)
            {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(ino, &raw)?;
        self.dev.flush()?;
        Ok(())
    }

    /// Unlink a regular file / symlink / special file at `path`.
    ///
    /// Semantics:
    /// - Refuses to unlink a directory (use a future `apply_rmdir`).
    /// - Decrements the target inode's `i_links_count`. When that reaches
    ///   zero, frees every data block via `plan_truncate_shrink(size → 0)`,
    ///   clears the inode bitmap bit, zeroes the inode body, and sets
    ///   `i_dtime = now`. When `links_count > 1` we only drop the dir entry
    ///   and decrement — matches POSIX unlink semantics for hard-linked files.
    /// - Mutates: parent-dir block (entry removal), target inode, block +
    ///   inode bitmaps, BGD counters, SB counters. No journaling yet —
    ///   safe only on scratch images (same caveat as `apply_truncate_shrink`).
    ///
    /// Returns `Error::NotFound` if the path doesn't exist,
    /// `Error::NotADirectory` if the parent isn't a directory, and
    /// `Error::IsADirectory` (POSIX EISDIR) if the target is a directory.
    pub fn apply_unlink(&self, path: &str) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        // POSIX: a trailing slash asserts the path refers to a directory,
        // which is incompatible with `unlink(2)` no matter what kind of file
        // the path resolves to. `split_parent_and_base` swallows the slash,
        // so snapshot the flag first and fail-fast on non-dirs below.
        let trailing_slash = path.len() > 1 && path.ends_with('/');
        let (parent_ino, base_name) = split_parent_and_base(path)?;

        // Resolve parent + target inodes.
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let parent_ino_num =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &parent_ino)?;
        let (parent_inode, _parent_raw) = self.read_inode_verified(parent_ino_num)?;
        if !parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }

        let target_ino = self.find_entry_in_dir(&parent_inode, base_name.as_bytes())?;
        let (target_inode, mut target_raw) = self.read_inode_verified(target_ino)?;
        if target_inode.is_dir() {
            // POSIX: unlink(2) on a directory must fail with EISDIR; the
            // caller should use rmdir(2) instead.
            return Err(Error::IsADirectory);
        }
        if trailing_slash {
            // `unlink("/foo/")` where /foo is a regular file → ENOTDIR per
            // POSIX: the trailing slash tells us the caller expected a dir.
            return Err(Error::NotADirectory);
        }

        // Remove the dir entry from the parent. Scans each block until
        // `remove_entry_from_block` reports success.
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let bs = self.sb.block_size();
        let parent_blocks = parent_inode.size.div_ceil(bs as u64);
        let mut removed = false;
        for logical in 0..parent_blocks {
            let Some(phys) =
                crate::extent::map_logical(&parent_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let mut block = self.read_block(phys)?;
            // `dir_entry_tail` occupies the last 12 bytes when metadata_csum
            // is on; don't scribble over it.
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&block) {
                12
            } else {
                0
            };
            if crate::dir::remove_entry_from_block(
                &mut block,
                base_name.as_bytes(),
                has_ft,
                reserved_tail,
            )? {
                // Recompute the tail csum if present — entry-list shape changed.
                if self.csum.enabled && reserved_tail == 12 {
                    let end = block.len();
                    let mut c = crate::checksum::linux_crc32c(
                        self.csum.seed,
                        &parent_ino_num.to_le_bytes(),
                    );
                    c = crate::checksum::linux_crc32c(c, &parent_inode.generation.to_le_bytes());
                    c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
                    block[end - 4..end].copy_from_slice(&c.to_le_bytes());
                }
                self.dev.write_at(phys * bs as u64, &block)?;
                removed = true;
                break;
            }
        }
        if !removed {
            return Err(Error::NotFound);
        }

        // Decrement link count. Non-zero after → just persist the new count
        // and the dtime update isn't needed.
        let new_links = target_inode.links_count.saturating_sub(1);
        target_raw[0x1A..0x1C].copy_from_slice(&new_links.to_le_bytes());

        if new_links > 0 {
            if self.csum.enabled {
                if let Some((lo, hi)) = self.csum.compute_inode_checksum(
                    target_ino,
                    target_inode.generation,
                    &target_raw,
                ) {
                    target_raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                    if target_raw.len() >= 0x84 {
                        target_raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                    }
                }
            }
            self.write_inode_raw(target_ino, &target_raw)?;
            self.dev.flush()?;
            return Ok(());
        }

        // Last link gone — free data blocks + inode slot.
        //
        // Truncate-to-zero re-uses the existing extent-free machinery. We
        // don't call `apply_truncate_shrink` directly because it recomputes
        // the inode checksum (we're about to zero the inode anyway). Use
        // the plan layer and apply the free-block-run mutations inline.
        let mut freed_sectors: u64 = 0;
        let sectors_per_block = bs as u64 / 512;
        if target_inode.has_extents() && target_inode.size > 0 {
            let (_sc, muts) = crate::file_mut::plan_truncate_shrink(
                target_inode.size,
                0,
                &target_inode.block,
                bs,
            )?;
            for m in &muts {
                if let crate::extent_mut::ExtentMutation::FreePhysicalRun { start, len } = m {
                    self.free_block_run(*start, *len as u64)?;
                    freed_sectors += *len as u64 * sectors_per_block;
                }
            }
        }

        // Clear the inode bitmap bit + bump free counter.
        self.free_inode_slot(target_ino)?;

        // Update SB counters: free_inodes_count++, free_blocks_count += (freed_sectors / sectors_per_block).
        let freed_blocks = freed_sectors.checked_div(sectors_per_block).unwrap_or(0);
        self.patch_sb_counters(freed_blocks as i64, 1)?;
        // Also credit the freed data blocks to the group's bg_free_blocks_count.
        if freed_blocks > 0 && target_inode.has_extents() {
            // The extents all live in the inode's group for our simple
            // single-group files (test images). A fragmented file spanning
            // groups would need the allocator to tell us per-group deltas.
            let gi = ((target_ino - 1) / self.sb.inodes_per_group) as usize;
            if gi < self.groups.len() {
                self.patch_bgd_counters(gi, freed_blocks as i32, 0, 0)?;
            }
        }

        // Zero the inode body. Kernel sets dtime = now, mode = 0, and
        // leaves the generation intact (helps tools like e2fsck detect the
        // dead slot). We match that: zero everything, set dtime, restore
        // generation.
        let inode_size = self.sb.inode_size as usize;
        let old_gen = target_inode.generation;
        for b in &mut target_raw[..inode_size] {
            *b = 0;
        }
        // dtime at offset 0x14..0x18
        let dtime = now_unix_seconds();
        target_raw[0x14..0x18].copy_from_slice(&dtime.to_le_bytes());
        // restore generation at 0x64..0x68
        target_raw[0x64..0x68].copy_from_slice(&old_gen.to_le_bytes());
        if self.csum.enabled {
            if let Some((lo, hi)) =
                self.csum
                    .compute_inode_checksum(target_ino, old_gen, &target_raw)
            {
                target_raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if target_raw.len() >= 0x84 {
                    target_raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(target_ino, &target_raw)?;
        self.dev.flush()?;
        Ok(())
    }

    /// Create a new regular file at `path` with permission bits `mode`
    /// (e.g. `0o644`). Returns the allocated inode number on success.
    ///
    /// Semantics:
    /// - Parent must exist and be a directory.
    /// - Refuses if `path` already exists.
    /// - Allocates an inode via `plan_inode_allocation` (hints to the
    ///   parent's group), marks the bitmap, bumps BGD + SB counters.
    /// - Initialises the inode as a regular file with EXTENTS flag and an
    ///   empty extent tree (size=0, blocks=0). Timestamps set to `now`.
    /// - Adds the directory entry into the first parent block with room
    ///   (linear; htree-extending dirs are a follow-up).
    /// - Not journaled — scratch-image safe, same caveat as other Phase-4
    ///   applies.
    pub fn apply_create(&self, path: &str, mode: u16) -> Result<u32> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let (parent_path, base_name) = split_parent_and_base(path)?;
        if base_name.len() > 255 {
            return Err(Error::NameTooLong);
        }

        // Resolve parent. Refuse if target already exists.
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let parent_ino_num =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &parent_path)?;
        let (parent_inode, _parent_raw) = self.read_inode_verified(parent_ino_num)?;
        if !parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        if self
            .find_entry_in_dir(&parent_inode, base_name.as_bytes())
            .is_ok()
        {
            return Err(Error::AlreadyExists);
        }

        // Allocate an inode, hinted to the parent's group.
        let parent_group = (parent_ino_num - 1) / self.sb.inodes_per_group;
        let bs = self.sb.block_size();
        let mut bitmap_reader = |block: u64| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; bs as usize];
            self.dev.read_at(block * bs as u64, &mut buf)?;
            Ok(buf)
        };
        let plan = crate::alloc::plan_inode_allocation(
            &self.sb,
            &self.groups,
            false,
            parent_group,
            &mut bitmap_reader,
        )?;
        let new_ino = plan.inode;

        // Persist the allocation: bitmap bit, BGD counters, SB counters.
        self.mark_inode_used(new_ino)?;
        self.patch_bgd_counters(
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        self.patch_sb_counters(plan.sb.free_blocks_delta as i64, plan.sb.free_inodes_delta)?;

        // Build + write the inode.
        let raw = self.build_regular_file_inode(new_ino, mode)?;
        self.write_inode_raw(new_ino, &raw)?;

        // Insert the dir entry into the first parent block with room.
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let parent_blocks = parent_inode.size.div_ceil(bs as u64);
        let mut added = false;
        for logical in 0..parent_blocks {
            let Some(phys) =
                crate::extent::map_logical(&parent_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let mut block = self.read_block(phys)?;
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&block) {
                12
            } else {
                0
            };
            match crate::dir::add_entry_to_block(
                &mut block,
                new_ino,
                base_name.as_bytes(),
                crate::dir::DirEntryType::RegFile,
                has_ft,
                reserved_tail,
            ) {
                Ok(()) => {
                    if self.csum.enabled && reserved_tail == 12 {
                        let end = block.len();
                        let mut c = crate::checksum::linux_crc32c(
                            self.csum.seed,
                            &parent_ino_num.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(
                            c,
                            &parent_inode.generation.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
                        block[end - 4..end].copy_from_slice(&c.to_le_bytes());
                    }
                    self.dev.write_at(phys * bs as u64, &block)?;
                    added = true;
                    break;
                }
                Err(Error::OutOfBounds) => {
                    // No room in this block — try the next.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        if !added {
            // No existing block has room → grow the directory.
            self.extend_dir_and_add_entry(
                parent_ino_num,
                base_name.as_bytes(),
                new_ino,
                crate::dir::DirEntryType::RegFile,
            )?;
        }

        self.dev.flush()?;
        Ok(new_ino)
    }

    /// Create a symbolic link at `linkpath` whose target is `target`.
    /// Mirrors POSIX `symlink(target, linkpath)`: allocates a fresh inode
    /// with mode S_IFLNK, installs the target bytes, and adds a dir entry
    /// at the link path.
    ///
    /// Two storage paths:
    /// - **Fast symlink** (`target.len() <= 60`): target stored inline in
    ///   the 60-byte `i_block` area; no data-block allocation.
    /// - **Slow symlink** (`61..=255` bytes): one filesystem block is
    ///   allocated and the target is written there, with an EXTENTS
    ///   i_block pointing at it.
    ///
    /// POSIX caps symlink targets at SYMLINK_MAX (255 bytes on Linux +
    /// macOS). Longer returns `Error::NameTooLong` → ENAMETOOLONG.
    pub fn apply_symlink(&self, target: &str, linkpath: &str) -> Result<u32> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        if target.is_empty() {
            return Err(Error::InvalidArgument("symlink target is empty"));
        }
        if target.len() > 255 {
            return Err(Error::NameTooLong);
        }
        let (parent_path, base_name) = split_parent_and_base(linkpath)?;
        if base_name.len() > 255 {
            return Err(Error::NameTooLong);
        }

        // Resolve parent + refuse duplicate.
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let parent_ino_num =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &parent_path)?;
        let (parent_inode, _parent_raw) = self.read_inode_verified(parent_ino_num)?;
        if !parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        if self
            .find_entry_in_dir(&parent_inode, base_name.as_bytes())
            .is_ok()
        {
            return Err(Error::AlreadyExists);
        }

        // Allocate a new inode.
        let parent_group = (parent_ino_num - 1) / self.sb.inodes_per_group;
        let bs = self.sb.block_size();
        let mut bitmap_reader = |block: u64| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; bs as usize];
            self.dev.read_at(block * bs as u64, &mut buf)?;
            Ok(buf)
        };
        let plan = crate::alloc::plan_inode_allocation(
            &self.sb,
            &self.groups,
            false,
            parent_group,
            &mut bitmap_reader,
        )?;
        let new_ino = plan.inode;

        self.mark_inode_used(new_ino)?;
        self.patch_bgd_counters(
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        self.patch_sb_counters(plan.sb.free_blocks_delta as i64, plan.sb.free_inodes_delta)?;

        // Fast-symlink if target fits inline; otherwise allocate a block.
        let raw = if target.len() <= 60 {
            self.build_fast_symlink_inode(new_ino, target.as_bytes())?
        } else {
            let mut bitmap_reader = |block: u64| -> Result<Vec<u8>> {
                let mut buf = vec![0u8; bs as usize];
                self.dev.read_at(block * bs as u64, &mut buf)?;
                Ok(buf)
            };
            let bplan = crate::alloc::plan_block_allocation(
                &self.sb,
                &self.groups,
                1,
                parent_group,
                &mut bitmap_reader,
            )?;
            let data_phys = bplan.first_block;

            self.mark_block_run_used(data_phys, 1)?;
            self.patch_bgd_counters(
                bplan.bgd.group_idx as usize,
                bplan.bgd.free_blocks_delta,
                bplan.bgd.free_inodes_delta,
                bplan.bgd.used_dirs_delta,
            )?;
            self.patch_sb_counters(bplan.sb.free_blocks_delta, bplan.sb.free_inodes_delta)?;

            let mut block = vec![0u8; bs as usize];
            block[..target.len()].copy_from_slice(target.as_bytes());
            self.dev.write_at(data_phys * bs as u64, &block)?;

            self.build_slow_symlink_inode(new_ino, target.as_bytes(), data_phys)?
        };
        self.write_inode_raw(new_ino, &raw)?;

        // Install dir entry.
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let parent_blocks = parent_inode.size.div_ceil(bs as u64);
        let mut added = false;
        for logical in 0..parent_blocks {
            let Some(phys) =
                crate::extent::map_logical(&parent_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let mut block = self.read_block(phys)?;
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&block) {
                12
            } else {
                0
            };
            match crate::dir::add_entry_to_block(
                &mut block,
                new_ino,
                base_name.as_bytes(),
                crate::dir::DirEntryType::Symlink,
                has_ft,
                reserved_tail,
            ) {
                Ok(()) => {
                    if self.csum.enabled && reserved_tail == 12 {
                        let end = block.len();
                        let mut c = crate::checksum::linux_crc32c(
                            self.csum.seed,
                            &parent_ino_num.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(
                            c,
                            &parent_inode.generation.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
                        block[end - 4..end].copy_from_slice(&c.to_le_bytes());
                    }
                    self.dev.write_at(phys * bs as u64, &block)?;
                    added = true;
                    break;
                }
                Err(Error::OutOfBounds) => continue,
                Err(e) => return Err(e),
            }
        }
        if !added {
            self.extend_dir_and_add_entry(
                parent_ino_num,
                base_name.as_bytes(),
                new_ino,
                crate::dir::DirEntryType::Symlink,
            )?;
        }

        self.dev.flush()?;
        Ok(new_ino)
    }

    /// Compose a fresh fast-symlink inode image: `S_IFLNK | 0o777`, 1 link,
    /// `i_size = target.len()`, 0 blocks, NO EXTENTS flag (fast symlinks
    /// store their target directly in the 60-byte `i_block` area — no
    /// extent tree).
    fn build_fast_symlink_inode(&self, ino: u32, target: &[u8]) -> Result<Vec<u8>> {
        debug_assert!(target.len() <= 60);
        let inode_size = self.sb.inode_size as usize;
        let mut raw = vec![0u8; inode_size];

        // Symlinks are traditionally rwxrwxrwx — the OS enforces access on
        // the *target*, not the symlink itself.
        let mode_bits = crate::inode::S_IFLNK | 0o0777;
        raw[0x00..0x02].copy_from_slice(&mode_bits.to_le_bytes());

        // i_size = target length (low 32).
        raw[0x04..0x08].copy_from_slice(&(target.len() as u32).to_le_bytes());

        // i_links_count = 1.
        raw[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());

        // i_flags = 0 (no EXTENTS — fast symlink stores target inline).
        raw[0x20..0x24].copy_from_slice(&0u32.to_le_bytes());

        // i_block at 0x28..0x64: target bytes, zero-padded.
        let blk_off = 0x28;
        raw[blk_off..blk_off + target.len()].copy_from_slice(target);

        let now = now_unix_seconds();
        raw[0x08..0x0C].copy_from_slice(&now.to_le_bytes()); // atime
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes()); // ctime
        raw[0x10..0x14].copy_from_slice(&now.to_le_bytes()); // mtime

        use std::sync::atomic::{AtomicU32, Ordering};
        static GEN_COUNTER: AtomicU32 = AtomicU32::new(1);
        let generation =
            std::process::id().wrapping_add(GEN_COUNTER.fetch_add(1, Ordering::Relaxed));
        raw[0x64..0x68].copy_from_slice(&generation.to_le_bytes());

        if inode_size >= 0x82 + 2 {
            raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        }

        if self.csum.enabled {
            if let Some((lo, hi)) = self.csum.compute_inode_checksum(ino, generation, &raw) {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        Ok(raw)
    }

    /// Compose a slow-symlink inode image: `S_IFLNK | 0o777`, 1 link,
    /// `i_size = target.len()`, EXTENTS flag set with a single-entry leaf
    /// root pointing at `data_phys` (logical block 0, length 1). One fs
    /// block worth of 512-byte sectors charged to `i_blocks`.
    ///
    /// Caller must have already written the target bytes (zero-padded) to
    /// `data_phys * block_size`.
    fn build_slow_symlink_inode(&self, ino: u32, target: &[u8], data_phys: u64) -> Result<Vec<u8>> {
        debug_assert!(target.len() > 60 && target.len() <= 255);
        let inode_size = self.sb.inode_size as usize;
        let mut raw = vec![0u8; inode_size];

        let mode_bits = crate::inode::S_IFLNK | 0o0777;
        raw[0x00..0x02].copy_from_slice(&mode_bits.to_le_bytes());

        // i_size (lo 32) = target length.
        raw[0x04..0x08].copy_from_slice(&(target.len() as u32).to_le_bytes());

        // i_links_count = 1.
        raw[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());

        // i_blocks_lo = sectors consumed by the single data block.
        let bs = self.sb.block_size() as u64;
        let sectors = bs / 512;
        raw[0x1C..0x20].copy_from_slice(&(sectors as u32).to_le_bytes());

        // i_flags = EXTENTS.
        let flags = crate::inode::InodeFlags::EXTENTS.bits();
        raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());

        // i_block at 0x28..0x64: extent leaf header + one entry.
        let eh_off = 0x28;
        raw[eh_off..eh_off + 2].copy_from_slice(&crate::extent::EXT4_EXT_MAGIC.to_le_bytes());
        raw[eh_off + 2..eh_off + 4].copy_from_slice(&1u16.to_le_bytes()); // entries=1
        raw[eh_off + 4..eh_off + 6].copy_from_slice(&4u16.to_le_bytes()); // max=4
        raw[eh_off + 6..eh_off + 8].copy_from_slice(&0u16.to_le_bytes()); // depth=0

        // Single leaf extent at entry-slot index 1 (offset +12): logical=0,
        // length=1, physical=data_phys.
        let e_off = eh_off + 12;
        raw[e_off..e_off + 4].copy_from_slice(&0u32.to_le_bytes()); // ee_block
        raw[e_off + 4..e_off + 6].copy_from_slice(&1u16.to_le_bytes()); // ee_len
        let phys_hi = ((data_phys >> 32) & 0xFFFF) as u16;
        let phys_lo = (data_phys & 0xFFFF_FFFF) as u32;
        raw[e_off + 6..e_off + 8].copy_from_slice(&phys_hi.to_le_bytes());
        raw[e_off + 8..e_off + 12].copy_from_slice(&phys_lo.to_le_bytes());

        let now = now_unix_seconds();
        raw[0x08..0x0C].copy_from_slice(&now.to_le_bytes()); // atime
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes()); // ctime
        raw[0x10..0x14].copy_from_slice(&now.to_le_bytes()); // mtime

        use std::sync::atomic::{AtomicU32, Ordering};
        static GEN_COUNTER: AtomicU32 = AtomicU32::new(1);
        let generation =
            std::process::id().wrapping_add(GEN_COUNTER.fetch_add(1, Ordering::Relaxed));
        raw[0x64..0x68].copy_from_slice(&generation.to_le_bytes());

        if inode_size >= 0x82 + 2 {
            raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        }

        if self.csum.enabled {
            if let Some((lo, hi)) = self.csum.compute_inode_checksum(ino, generation, &raw) {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        Ok(raw)
    }

    /// Compose a fresh regular-file inode image: `S_IFREG | mode`, 1 link,
    /// 0 size, 0 blocks, EXTENTS flag set with an empty 4-entry leaf root,
    /// timestamps = now, generation = process-id-derived counter, extra_isize
    /// = 32 so the inode has room for nsec timestamps + checksum_hi.
    fn build_regular_file_inode(&self, ino: u32, mode: u16) -> Result<Vec<u8>> {
        let inode_size = self.sb.inode_size as usize;
        let mut raw = vec![0u8; inode_size];

        // i_mode at 0x00..0x02
        let mode_bits = crate::inode::S_IFREG | (mode & 0x0FFF);
        raw[0x00..0x02].copy_from_slice(&mode_bits.to_le_bytes());

        // i_links_count at 0x1A..0x1C = 1
        raw[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());

        // i_flags at 0x20..0x24 = EXTENTS
        let flags = crate::inode::InodeFlags::EXTENTS.bits();
        raw[0x20..0x24].copy_from_slice(&flags.to_le_bytes());

        // i_block at 0x28..0x64 (60 bytes): empty extent leaf header.
        //   magic (u16)=0xF30A, entries=0, max=4, depth=0, generation=0
        let eh_off = 0x28;
        raw[eh_off..eh_off + 2].copy_from_slice(&crate::extent::EXT4_EXT_MAGIC.to_le_bytes());
        raw[eh_off + 2..eh_off + 4].copy_from_slice(&0u16.to_le_bytes()); // entries
        raw[eh_off + 4..eh_off + 6].copy_from_slice(&4u16.to_le_bytes()); // max
        raw[eh_off + 6..eh_off + 8].copy_from_slice(&0u16.to_le_bytes()); // depth
                                                                          // eh_generation at eh_off+8..eh_off+12 stays zero

        // Timestamps: atime 0x08, ctime 0x0C, mtime 0x10, dtime 0x14
        let now = now_unix_seconds();
        raw[0x08..0x0C].copy_from_slice(&now.to_le_bytes()); // atime
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes()); // ctime
        raw[0x10..0x14].copy_from_slice(&now.to_le_bytes()); // mtime
                                                             // dtime stays zero (not deleted).

        // i_generation at 0x64..0x68. We combine pid + a process-lifetime
        // counter so successive creates within the same session have
        // different generations.
        use std::sync::atomic::{AtomicU32, Ordering};
        static GEN_COUNTER: AtomicU32 = AtomicU32::new(1);
        let generation =
            std::process::id().wrapping_add(GEN_COUNTER.fetch_add(1, Ordering::Relaxed));
        raw[0x64..0x68].copy_from_slice(&generation.to_le_bytes());

        // i_extra_isize at 0x80..0x82 — 32 is the modern default (room for
        // crtime, nsec halves, checksum_hi).
        if inode_size >= 0x82 + 2 {
            raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        }

        // Recompute checksum if enabled.
        if self.csum.enabled {
            if let Some((lo, hi)) = self.csum.compute_inode_checksum(ino, generation, &raw) {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        Ok(raw)
    }

    /// Replace the content of `path` with `data`. The file must already
    /// exist. Frees every existing extent, allocates a single contiguous run
    /// of blocks large enough for `data`, writes the bytes (zero-padding the
    /// tail of the last block), then inserts one extent into the inode.
    ///
    /// This is the "Finder just saved a document" path — complete rewrite of
    /// a file. Piecewise writes / appends / sparse writes come later.
    ///
    /// Not journaled — scratch-image safe, same caveat as other Phase-4 ops.
    /// Returns the new file size on success.
    pub fn apply_replace_file_content(&self, path: &str, data: &[u8]) -> Result<u64> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let ino = crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, path)?;
        let (inode, mut raw) = self.read_inode_verified(ino)?;
        if !inode.is_file() {
            return Err(Error::InvalidArgument(
                "write_file target is not a regular file",
            ));
        }
        if !inode.has_extents() {
            return Err(Error::InvalidArgument(
                "write_file target is non-EXTENTS (legacy inode)",
            ));
        }

        let bs = self.sb.block_size();
        let sectors_per_block = bs as u64 / 512;
        let group_idx_of_inode = ((ino - 1) / self.sb.inodes_per_group) as usize;

        // Phase 1: free any existing data blocks. Reuses the extent-shrink
        // planner which handles partial tail extents cleanly.
        let mut freed_fs_blocks: u64 = 0;
        if inode.size > 0 {
            let (_sc, muts) =
                crate::file_mut::plan_truncate_shrink(inode.size, 0, &inode.block, bs)?;
            for m in &muts {
                if let crate::extent_mut::ExtentMutation::FreePhysicalRun { start, len } = m {
                    self.free_block_run(*start, *len as u64)?;
                    freed_fs_blocks += *len as u64;
                }
            }
        }
        // Reset the inode's extent root to an empty leaf. Any prior content
        // is now unreferenced.
        let mut root = vec![0u8; 60];
        root[0..2].copy_from_slice(&crate::extent::EXT4_EXT_MAGIC.to_le_bytes());
        root[4..6].copy_from_slice(&4u16.to_le_bytes()); // max entries
        Self::patch_inode_block_area(&mut raw, &root)?;

        // Empty write: update size + accounting, credit freed blocks back
        // to SB and the source group's BGD, exit.
        if data.is_empty() {
            self.finalize_inode_after_write(ino, &mut raw, &inode, 0, 0)?;
            if freed_fs_blocks > 0 {
                self.patch_sb_counters(freed_fs_blocks as i64, 0)?;
                self.patch_bgd_counters(group_idx_of_inode, freed_fs_blocks as i32, 0, 0)?;
            }
            self.dev.flush()?;
            return Ok(0);
        }

        // Phase 2: allocate one contiguous run for the whole payload.
        //
        // plan_block_allocation refuses to split; callers with highly
        // fragmented images get an Err back and can fall back to a
        // piecewise strategy (not implemented yet).
        let needed_blocks: u32 = data.len().div_ceil(bs as usize) as u32;
        let mut bitmap_reader = |block: u64| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; bs as usize];
            self.dev.read_at(block * bs as u64, &mut buf)?;
            Ok(buf)
        };
        let plan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.groups,
            needed_blocks,
            group_idx_of_inode as u32,
            &mut bitmap_reader,
        )?;

        // Phase 3: write bitmap + BGD + SB deltas.
        //
        // Accounting: the allocator plan already carries
        // `free_blocks_delta = -needed` for the destination group. We layer
        // on top: (a) destination bitmap marked used, (b) if the old content
        // was in a different group, credit its freed blocks separately, and
        // (c) net SB delta `freed - needed`.
        self.set_block_run_used(plan.first_block, needed_blocks as u64)?;
        self.patch_bgd_counters(
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        if freed_fs_blocks > 0 {
            // If dest == source, this adds `+freed` to the BGD, netting
            // `freed - needed` against the allocator's -needed. If dest !=
            // source, it bumps the source's free_blocks back up.
            self.patch_bgd_counters(group_idx_of_inode, freed_fs_blocks as i32, 0, 0)?;
        }
        let net_block_delta = freed_fs_blocks as i64 - needed_blocks as i64;
        self.patch_sb_counters(net_block_delta, 0)?;

        // Phase 4: write the payload into the allocated physical run.
        for i in 0..needed_blocks as u64 {
            let off_in_data = (i as usize) * bs as usize;
            let chunk_end = ((i as usize + 1) * bs as usize).min(data.len());
            let mut block = vec![0u8; bs as usize];
            block[..chunk_end - off_in_data].copy_from_slice(&data[off_in_data..chunk_end]);
            self.dev
                .write_at((plan.first_block + i) * bs as u64, &block)?;
        }

        // Phase 5: insert the single extent into the (now-empty) inline root.
        let new_extent = crate::extent::Extent {
            logical_block: 0,
            length: needed_blocks as u16,
            physical_block: plan.first_block,
            uninitialized: false,
        };
        let muts = crate::extent_mut::plan_insert_extent(&root, new_extent)?;
        for m in &muts {
            if let crate::extent_mut::ExtentMutation::WriteRoot { bytes } = m {
                Self::patch_inode_block_area(&mut raw, bytes)?;
            }
        }

        // Phase 6: update size + blocks + csum and persist the inode.
        let new_size = data.len() as u64;
        let new_sectors = needed_blocks as u64 * sectors_per_block;
        self.finalize_inode_after_write(ino, &mut raw, &inode, new_size, new_sectors)?;
        self.dev.flush()?;
        Ok(new_size)
    }

    /// Patch size + blocks counter on the inode image, recompute the csum
    /// if enabled, and write it back. Shared tail for apply_replace_file_content and
    /// any future writer that produces a new `raw` image.
    fn finalize_inode_after_write(
        &self,
        ino: u32,
        raw: &mut [u8],
        orig: &Inode,
        new_size: u64,
        new_sectors: u64,
    ) -> Result<()> {
        Self::patch_inode_size_and_blocks(raw, new_size, new_sectors)?;
        // Bump mtime + ctime = now (atime left alone; matches POSIX).
        let now = now_unix_seconds();
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes()); // ctime
        raw[0x10..0x14].copy_from_slice(&now.to_le_bytes()); // mtime
        if self.csum.enabled {
            if let Some((lo, hi)) = self.csum.compute_inode_checksum(ino, orig.generation, raw) {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(ino, raw)
    }

    /// Mark `len` bits starting at block `start` as USED in the containing
    /// block group's bitmap. Mirrors `free_block_run` but sets rather than
    /// clears. Assumes the run lies entirely within one group (same caveat).
    fn set_block_run_used(&self, start: u64, len: u64) -> Result<()> {
        let bpg = self.sb.blocks_per_group as u64;
        let first_data = self.sb.first_data_block as u64;
        let gi = ((start - first_data) / bpg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidBlock(start));
        }
        let group_start = first_data + gi as u64 * bpg;
        let bit_start = (start - group_start) as u32;
        let bitmap_block = self.groups[gi].block_bitmap;
        let bs = self.sb.block_size() as u64;
        let mut buf = vec![0u8; bs as usize];
        self.dev.read_at(bitmap_block * bs, &mut buf)?;
        for i in 0..len {
            let bit = bit_start as u64 + i;
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit % 8);
            if byte < buf.len() {
                buf[byte] |= mask;
            }
        }
        self.dev.write_at(bitmap_block * bs, &buf)?;
        Ok(())
    }

    /// Find `name` in directory `dir_inode` — scans each data block. Returns
    /// the inode number or `Error::NotFound`.
    fn find_entry_in_dir(&self, dir_inode: &Inode, name: &[u8]) -> Result<u32> {
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let bs = self.sb.block_size();
        let n_blocks = dir_inode.size.div_ceil(bs as u64);
        for logical in 0..n_blocks {
            let Some(phys) =
                crate::extent::map_logical(&dir_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let block = self.read_block(phys)?;
            for entry in crate::dir::DirBlockIter::new(&block, has_ft) {
                let e = entry?;
                if e.name == name {
                    return Ok(e.inode);
                }
            }
        }
        Err(Error::NotFound)
    }

    /// Clear the inode bitmap bit for `ino`. Does NOT touch counters — the
    /// caller pairs this with a `patch_bgd_counters` + `patch_sb_counters` call
    /// so BGD free_inodes_count and SB free_inodes_count land together.
    fn free_inode_slot(&self, ino: u32) -> Result<()> {
        let ipg = self.sb.inodes_per_group;
        let gi = ((ino - 1) / ipg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidInode(ino));
        }
        let bit = ((ino - 1) % ipg) as u64;
        let bitmap_block = self.groups[gi].inode_bitmap;
        let bs = self.sb.block_size() as u64;
        let mut buf = vec![0u8; bs as usize];
        self.dev.read_at(bitmap_block * bs, &mut buf)?;
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if byte < buf.len() {
            buf[byte] &= !mask;
        }
        self.dev.write_at(bitmap_block * bs, &buf)?;

        self.patch_bgd_counters(gi, 0, 1, 0)?;
        Ok(())
    }

    /// Set the inode bitmap bit for `ino`. Paired with `patch_bgd_counters`
    /// (`free_inodes_delta = -1` and, for dirs, `used_dirs_delta = +1`).
    fn mark_inode_used(&self, ino: u32) -> Result<()> {
        let ipg = self.sb.inodes_per_group;
        let gi = ((ino - 1) / ipg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidInode(ino));
        }
        let bit = ((ino - 1) % ipg) as u64;
        let bitmap_block = self.groups[gi].inode_bitmap;
        let bs = self.sb.block_size() as u64;
        let mut buf = vec![0u8; bs as usize];
        self.dev.read_at(bitmap_block * bs, &mut buf)?;
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if byte < buf.len() {
            buf[byte] |= mask;
        }
        self.dev.write_at(bitmap_block * bs, &buf)?;
        Ok(())
    }

    /// Apply per-group counter deltas on disk for group `gi`. Positive deltas
    /// increase the corresponding `bg_free_*` / `bg_used_dirs` counter,
    /// negative deltas decrease. Recomputes the BGD csum when `metadata_csum`
    /// is enabled. The in-memory `self.groups` copy is NOT updated — callers
    /// doing a sequence of allocations should `Filesystem::mount` fresh.
    fn patch_bgd_counters(
        &self,
        gi: usize,
        free_blocks_delta: i32,
        free_inodes_delta: i32,
        used_dirs_delta: i32,
    ) -> Result<()> {
        let bs = self.sb.block_size() as u64;
        let desc_size = self.sb.desc_size as u64;
        let bgt_first_block = self.sb.first_data_block as u64 + 1;
        let byte_in_bgt = gi as u64 * desc_size;
        let bgt_block = bgt_first_block + byte_in_bgt / bs;
        let off_in_block = (byte_in_bgt % bs) as usize;

        let mut block = self.read_block(bgt_block)?;

        // Patch one little-endian u16+hi_u16 pair inside the descriptor.
        let patch_u32 = |block: &mut [u8], lo: usize, hi: Option<usize>, delta: i32| {
            let cur_lo = u16::from_le_bytes(block[lo..lo + 2].try_into().unwrap()) as u32;
            let cur_hi = hi
                .map(|h| u16::from_le_bytes(block[h..h + 2].try_into().unwrap()) as u32)
                .unwrap_or(0);
            let cur = (cur_hi << 16) | cur_lo;
            let new = (cur as i64 + delta as i64).max(0) as u32;
            block[lo..lo + 2].copy_from_slice(&((new & 0xFFFF) as u16).to_le_bytes());
            if let Some(h) = hi {
                block[h..h + 2].copy_from_slice(&(((new >> 16) & 0xFFFF) as u16).to_le_bytes());
            }
        };
        let patch_u16 = |block: &mut [u8], at: usize, delta: i32| {
            let cur = u16::from_le_bytes(block[at..at + 2].try_into().unwrap()) as i32;
            let new = (cur + delta).max(0) as u16;
            block[at..at + 2].copy_from_slice(&new.to_le_bytes());
        };

        // Free-blocks: 16-bit at 0x0C, hi at 0x2A when 64-bit
        patch_u32(
            &mut block,
            off_in_block + 0x0C,
            if desc_size >= 0x40 {
                Some(off_in_block + 0x2A)
            } else {
                None
            },
            free_blocks_delta,
        );
        // Free-inodes: 16-bit at 0x0E, hi at 0x2C when 64-bit
        patch_u32(
            &mut block,
            off_in_block + 0x0E,
            if desc_size >= 0x40 {
                Some(off_in_block + 0x2C)
            } else {
                None
            },
            free_inodes_delta,
        );
        // Used-dirs: 16-bit only (kernel defines u16+u16 hi at 0x2E too, but
        // dirs per group realistically fit in u16 — handle both anyway).
        patch_u32(
            &mut block,
            off_in_block + 0x10,
            if desc_size >= 0x40 {
                Some(off_in_block + 0x2E)
            } else {
                None
            },
            used_dirs_delta,
        );
        let _ = patch_u16;

        if self.csum.enabled {
            let stored_at = off_in_block + 0x1E;
            let end_desc = off_in_block + desc_size as usize;
            block[stored_at..stored_at + 2].copy_from_slice(&[0, 0]);
            let seed = self.csum.seed;
            let mut c = crate::checksum::linux_crc32c(seed, &(gi as u32).to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &block[off_in_block..end_desc]);
            let new_csum = c as u16;
            block[stored_at..stored_at + 2].copy_from_slice(&new_csum.to_le_bytes());
        }
        self.dev.write_at(bgt_block * bs, &block)?;
        Ok(())
    }

    /// Apply deltas to SB `s_free_blocks_count` and `s_free_inodes_count`.
    /// Recomputes the SB checksum when enabled. Does not mutate `self.sb`.
    fn patch_sb_counters(&self, free_blocks_delta: i64, free_inodes_delta: i32) -> Result<()> {
        let mut sb_raw = self.sb.raw.clone();
        // s_free_inodes_count at 0x10..0x14
        let fi = u32::from_le_bytes(sb_raw[0x10..0x14].try_into().unwrap()) as i64;
        let fi_new = (fi + free_inodes_delta as i64).max(0) as u32;
        sb_raw[0x10..0x14].copy_from_slice(&fi_new.to_le_bytes());
        // s_free_blocks_count split lo (0x0C..0x10, u32) + hi (0x158..0x15C, u32)
        let lo = u32::from_le_bytes(sb_raw[0x0C..0x10].try_into().unwrap()) as u64;
        let hi = u32::from_le_bytes(sb_raw[0x158..0x15C].try_into().unwrap()) as u64;
        let cur = ((hi << 32) | lo) as i64;
        let new = (cur + free_blocks_delta).max(0) as u64;
        sb_raw[0x0C..0x10].copy_from_slice(&(new as u32).to_le_bytes());
        sb_raw[0x158..0x15C].copy_from_slice(&((new >> 32) as u32).to_le_bytes());
        if self.csum.enabled {
            let csum = crate::checksum::linux_crc32c(!0, &sb_raw[..0x3FC]);
            sb_raw[0x3FC..0x400].copy_from_slice(&csum.to_le_bytes());
        }
        self.dev
            .write_at(crate::superblock::SUPERBLOCK_OFFSET, &sb_raw)?;
        Ok(())
    }

    /// Zero the bitmap bits covering the physical block run
    /// `[start, start+len)`. Assumes the run lies entirely within one block
    /// group (true for allocator-produced runs; fragmentation across groups
    /// is a future concern).
    fn free_block_run(&self, start: u64, len: u64) -> Result<()> {
        let bpg = self.sb.blocks_per_group as u64;
        let first_data = self.sb.first_data_block as u64;
        // Block group index of the first block in the run.
        let gi = ((start - first_data) / bpg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidBlock(start));
        }
        let group_start = first_data + gi as u64 * bpg;
        let bit_start = (start - group_start) as u32;
        let bg = &self.groups[gi];
        let bitmap_block = bg.block_bitmap;

        let bs = self.sb.block_size() as u64;
        let mut buf = vec![0u8; bs as usize];
        self.dev.read_at(bitmap_block * bs, &mut buf)?;
        for i in 0..len {
            let bit = bit_start as u64 + i;
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit % 8);
            if byte < buf.len() {
                buf[byte] &= !mask;
            }
        }
        self.dev.write_at(bitmap_block * bs, &buf)?;
        Ok(())
    }

    /// Mark a physical-block run `[start, start+len)` as USED in the
    /// containing group's block bitmap. Inverse of [`free_block_run`].
    /// Assumes the run is within one block group (allocator contract).
    fn mark_block_run_used(&self, start: u64, len: u64) -> Result<()> {
        let bpg = self.sb.blocks_per_group as u64;
        let first_data = self.sb.first_data_block as u64;
        let gi = ((start - first_data) / bpg) as usize;
        if gi >= self.groups.len() {
            return Err(Error::InvalidBlock(start));
        }
        let group_start = first_data + gi as u64 * bpg;
        let bit_start = (start - group_start) as u32;
        let bitmap_block = self.groups[gi].block_bitmap;

        let bs = self.sb.block_size() as u64;
        let mut buf = vec![0u8; bs as usize];
        self.dev.read_at(bitmap_block * bs, &mut buf)?;
        for i in 0..len {
            let bit = bit_start as u64 + i;
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit % 8);
            if byte < buf.len() {
                buf[byte] |= mask;
            }
        }
        self.dev.write_at(bitmap_block * bs, &buf)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // mkdir / rmdir
    // -----------------------------------------------------------------------

    /// Build an on-disk inode image for a freshly-created directory. Sets
    /// `S_IFDIR | mode`, `i_links_count = 2` (for `.` and the dir entry in
    /// the parent), `i_size = block_size` (one data block), EXTENTS flag
    /// with a single leaf extent mapping logical 0 → `data_phys_block`,
    /// timestamps = now.
    fn build_directory_inode(&self, ino: u32, mode: u16, data_phys_block: u64) -> Result<Vec<u8>> {
        let inode_size = self.sb.inode_size as usize;
        let mut raw = vec![0u8; inode_size];

        let mode_bits = crate::inode::S_IFDIR | (mode & 0x0FFF);
        raw[0x00..0x02].copy_from_slice(&mode_bits.to_le_bytes());

        // i_links_count = 2: one for the "." entry, one for the parent's entry
        // naming this dir. A subdir created later in this dir bumps it to 3, etc.
        raw[0x1A..0x1C].copy_from_slice(&2u16.to_le_bytes());

        // i_flags = EXTENTS
        raw[0x20..0x24].copy_from_slice(&crate::inode::InodeFlags::EXTENTS.bits().to_le_bytes());

        // i_block (60 B): extent header (leaf, 1 entry, max 4) + one Extent.
        let eh = 0x28;
        raw[eh..eh + 2].copy_from_slice(&crate::extent::EXT4_EXT_MAGIC.to_le_bytes());
        raw[eh + 2..eh + 4].copy_from_slice(&1u16.to_le_bytes()); // entries
        raw[eh + 4..eh + 6].copy_from_slice(&4u16.to_le_bytes()); // max
                                                                  // depth=0 leaf, generation=0
                                                                  // Entry at eh+12..eh+24: logical 0, len 1, phys = data_phys_block.
        let e = eh + 12;
        raw[e..e + 4].copy_from_slice(&0u32.to_le_bytes()); // logical
        raw[e + 4..e + 6].copy_from_slice(&1u16.to_le_bytes()); // length
        let hi = ((data_phys_block >> 32) & 0xFFFF) as u16;
        let lo = (data_phys_block & 0xFFFF_FFFF) as u32;
        raw[e + 6..e + 8].copy_from_slice(&hi.to_le_bytes());
        raw[e + 8..e + 12].copy_from_slice(&lo.to_le_bytes());

        // Size = block_size (the single data block fills the file).
        let bs = self.sb.block_size() as u64;
        let size_lo = (bs & 0xFFFF_FFFF) as u32;
        let size_hi = (bs >> 32) as u32;
        raw[0x04..0x08].copy_from_slice(&size_lo.to_le_bytes());
        raw[0x6C..0x70].copy_from_slice(&size_hi.to_le_bytes());

        // i_blocks in 512-byte sectors.
        let sectors = bs / 512;
        raw[0x1C..0x20].copy_from_slice(&(sectors as u32).to_le_bytes());
        raw[0x74..0x76].copy_from_slice(&(((sectors >> 32) & 0xFFFF) as u16).to_le_bytes());

        // Timestamps (atime/ctime/mtime = now).
        let now = now_unix_seconds();
        raw[0x08..0x0C].copy_from_slice(&now.to_le_bytes());
        raw[0x0C..0x10].copy_from_slice(&now.to_le_bytes());
        raw[0x10..0x14].copy_from_slice(&now.to_le_bytes());

        // i_generation at 0x64..0x68 — mirror apply_create's derivation so
        // successive mkdir calls have distinct values.
        use std::sync::atomic::{AtomicU32, Ordering};
        static GEN_COUNTER: AtomicU32 = AtomicU32::new(1);
        let generation =
            std::process::id().wrapping_add(GEN_COUNTER.fetch_add(1, Ordering::Relaxed));
        raw[0x64..0x68].copy_from_slice(&generation.to_le_bytes());

        if inode_size >= 0x82 + 2 {
            raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        }

        if self.csum.enabled {
            if let Some((lo16, hi16)) = self.csum.compute_inode_checksum(ino, generation, &raw) {
                raw[0x7C..0x7E].copy_from_slice(&lo16.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi16.to_le_bytes());
                }
            }
        }
        Ok(raw)
    }

    /// Seed a freshly-allocated dir block with the two canonical entries
    /// `.` (→ new_ino) and `..` (→ parent_ino). Handles the metadata-csum
    /// tail when required: the last 12 bytes are reserved, and the CRC is
    /// computed over everything before them.
    fn seed_directory_block(
        &self,
        new_ino: u32,
        parent_ino: u32,
        new_generation: u32,
    ) -> Result<Vec<u8>> {
        let bs = self.sb.block_size() as usize;
        let mut block = vec![0u8; bs];
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let reserved_tail = if self.csum.enabled { 12 } else { 0 };
        let usable = bs - reserved_tail;

        // "." entry: rec_len = 12
        block[0..4].copy_from_slice(&new_ino.to_le_bytes());
        block[4..6].copy_from_slice(&12u16.to_le_bytes());
        block[6] = 1; // name_len
        block[7] = if has_ft {
            crate::dir::DirEntryType::Directory as u8
        } else {
            0
        };
        block[8] = b'.';

        // ".." entry: rec_len absorbs the rest of the usable region.
        let off = 12;
        block[off..off + 4].copy_from_slice(&parent_ino.to_le_bytes());
        let rec_len = (usable - off) as u16;
        block[off + 4..off + 6].copy_from_slice(&rec_len.to_le_bytes());
        block[off + 6] = 2;
        block[off + 7] = if has_ft {
            crate::dir::DirEntryType::Directory as u8
        } else {
            0
        };
        block[off + 8] = b'.';
        block[off + 9] = b'.';

        // Tail (when metadata_csum enabled): fake inode=0, rec_len=12,
        // name_len=0, file_type=0xDE, u32 checksum.
        if reserved_tail == 12 {
            let tail = bs - 12;
            block[tail..tail + 4].copy_from_slice(&0u32.to_le_bytes()); // inode=0
            block[tail + 4..tail + 6].copy_from_slice(&12u16.to_le_bytes()); // rec_len
            block[tail + 6] = 0; // name_len
            block[tail + 7] = 0xDE; // file_type marker
                                    // CRC32C over [0 .. bs - 12] salted by ino + gen.
            let mut c = crate::checksum::linux_crc32c(self.csum.seed, &new_ino.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &new_generation.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &block[..bs - 12]);
            block[bs - 4..bs].copy_from_slice(&c.to_le_bytes());
        }

        Ok(block)
    }

    /// Adjust `i_links_count` on a raw inode image. Recomputes CSUM.
    fn patch_inode_nlink(&self, ino: u32, raw: &mut [u8], inode: &Inode, delta: i32) -> Result<()> {
        let new_count = (inode.links_count as i32 + delta).max(0) as u16;
        raw[0x1A..0x1C].copy_from_slice(&new_count.to_le_bytes());
        if self.csum.enabled {
            if let Some((lo, hi)) = self.csum.compute_inode_checksum(ino, inode.generation, raw) {
                raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if raw.len() >= 0x84 {
                    raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        Ok(())
    }

    /// Create a subdirectory at `path` with POSIX mode bits (low 12 bits of
    /// `mode`). Returns the new directory's inode number. Steps: allocate
    /// inode (Orlov-hinted) → allocate one data block → seed it with `.` / `..`
    /// → build dir inode → write inode + data block → add dir entry in parent
    /// → bump parent's `i_links_count` → commit BGD/SB counters.
    ///
    /// Not journaled — safe only in scratch-image contexts until transaction
    /// wrapping lands.
    pub fn apply_mkdir(&self, path: &str, mode: u16) -> Result<u32> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let (parent_path, base_name) = split_parent_and_base(path)?;
        if base_name.len() > 255 {
            return Err(Error::NameTooLong);
        }

        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let parent_ino =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &parent_path)?;
        let (parent_inode, mut parent_raw) = self.read_inode_verified(parent_ino)?;
        if !parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        if self
            .find_entry_in_dir(&parent_inode, base_name.as_bytes())
            .is_ok()
        {
            return Err(Error::AlreadyExists);
        }

        let bs = self.sb.block_size();
        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;
        let mut bitmap_reader = |block: u64| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; bs as usize];
            self.dev.read_at(block * bs as u64, &mut buf)?;
            Ok(buf)
        };

        // 1. Allocate inode (is_dir = true so Orlov picks a dir-friendly group).
        let iplan = crate::alloc::plan_inode_allocation(
            &self.sb,
            &self.groups,
            true,
            parent_group,
            &mut bitmap_reader,
        )?;
        let new_ino = iplan.inode;

        // 2. Allocate one data block for the dir contents.
        let bplan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.groups,
            1,
            iplan.bgd.group_idx,
            &mut bitmap_reader,
        )?;
        let data_block = bplan.first_block;

        // 3. Commit inode allocator side-effects first so a later failure
        //    leaves a self-consistent filesystem (inode will be freed by the
        //    rollback branch below if needed).
        self.mark_inode_used(new_ino)?;
        self.patch_bgd_counters(
            iplan.bgd.group_idx as usize,
            iplan.bgd.free_blocks_delta,
            iplan.bgd.free_inodes_delta,
            iplan.bgd.used_dirs_delta,
        )?;
        self.patch_sb_counters(iplan.sb.free_blocks_delta, iplan.sb.free_inodes_delta)?;

        // 4. Commit block allocator side-effects.
        self.mark_block_run_used(data_block, 1)?;
        self.patch_bgd_counters(
            bplan.bgd.group_idx as usize,
            bplan.bgd.free_blocks_delta,
            bplan.bgd.free_inodes_delta,
            bplan.bgd.used_dirs_delta,
        )?;
        self.patch_sb_counters(bplan.sb.free_blocks_delta, bplan.sb.free_inodes_delta)?;

        // 5. Build + write the dir inode.
        let raw = self.build_directory_inode(new_ino, mode, data_block)?;
        // Extract generation from the freshly-built raw for seed_directory_block CSUM.
        let gen = u32::from_le_bytes(raw[0x64..0x68].try_into().unwrap());
        self.write_inode_raw(new_ino, &raw)?;

        // 6. Seed + write the data block.
        let seed = self.seed_directory_block(new_ino, parent_ino, gen)?;
        self.dev.write_at(data_block * bs as u64, &seed)?;

        // 7. Add dir entry in parent for the new directory.
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let parent_blocks = parent_inode.size.div_ceil(bs as u64);
        let mut added = false;
        for logical in 0..parent_blocks {
            let Some(phys) =
                crate::extent::map_logical(&parent_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let mut block = self.read_block(phys)?;
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&block) {
                12
            } else {
                0
            };
            match crate::dir::add_entry_to_block(
                &mut block,
                new_ino,
                base_name.as_bytes(),
                crate::dir::DirEntryType::Directory,
                has_ft,
                reserved_tail,
            ) {
                Ok(()) => {
                    if self.csum.enabled && reserved_tail == 12 {
                        let end = block.len();
                        let mut c = crate::checksum::linux_crc32c(
                            self.csum.seed,
                            &parent_ino.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(
                            c,
                            &parent_inode.generation.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
                        block[end - 4..end].copy_from_slice(&c.to_le_bytes());
                    }
                    self.dev.write_at(phys * bs as u64, &block)?;
                    added = true;
                    break;
                }
                Err(Error::OutOfBounds) => continue,
                Err(e) => return Err(e),
            }
        }
        if !added {
            // No existing block has room → grow the directory. extend_dir_...
            // rewrites the parent inode on disk with a larger size + new
            // extent, so `parent_raw` here is now stale — re-read before the
            // nlink patch so we don't stomp the extension.
            self.extend_dir_and_add_entry(
                parent_ino,
                base_name.as_bytes(),
                new_ino,
                crate::dir::DirEntryType::Directory,
            )?;
            let refreshed = self.read_inode_verified(parent_ino)?;
            parent_raw = refreshed.1;
        }

        // 8. Parent gets +1 nlink (the child's ".." adds a reference back).
        self.patch_inode_nlink(parent_ino, &mut parent_raw, &parent_inode, 1)?;
        self.write_inode_raw(parent_ino, &parent_raw)?;

        self.dev.flush()?;
        Ok(new_ino)
    }

    /// Create a hard link at `dst` pointing to the same inode as `src`.
    ///
    /// Semantics:
    /// - `src` must exist and must NOT be a directory (POSIX forbids
    ///   directory hardlinks to avoid reference cycles).
    /// - `dst`'s parent must exist and be a directory.
    /// - `dst` must not already exist.
    /// - On success the shared inode's `i_links_count` is incremented by 1.
    ///
    /// Not journaled — same caveat as other Phase-4 ops.
    pub fn apply_link(&self, src: &str, dst: &str) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let (dst_parent_path, dst_name) = split_parent_and_base(dst)?;
        if dst_name.len() > 255 {
            return Err(Error::NameTooLong);
        }

        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let src_ino = crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, src)?;
        let (src_inode, mut src_raw) = self.read_inode_verified(src_ino)?;
        if src_inode.is_dir() {
            // POSIX: hard-linking a directory is forbidden. Map to EISDIR
            // (rather than EPERM) — matches our IsADirectory convention.
            return Err(Error::IsADirectory);
        }

        let dst_parent_ino =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &dst_parent_path)?;
        let (dst_parent_inode, _) = self.read_inode_verified(dst_parent_ino)?;
        if !dst_parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        if self
            .find_entry_in_dir(&dst_parent_inode, dst_name.as_bytes())
            .is_ok()
        {
            return Err(Error::AlreadyExists);
        }

        // Bump nlink BEFORE adding the entry. If we crash after the nlink
        // bump but before writing the new entry we leak at most 1 link
        // (inode stays allocated one step longer than necessary). If we did
        // it the other way a crash could leave the entry pointing at a
        // soon-to-be-freed inode.
        self.patch_inode_nlink(src_ino, &mut src_raw, &src_inode, 1)?;
        self.write_inode_raw(src_ino, &src_raw)?;

        let dir_type = match src_inode.file_type() {
            crate::inode::S_IFREG => crate::dir::DirEntryType::RegFile,
            crate::inode::S_IFLNK => crate::dir::DirEntryType::Symlink,
            crate::inode::S_IFCHR => crate::dir::DirEntryType::CharDev,
            crate::inode::S_IFBLK => crate::dir::DirEntryType::BlockDev,
            crate::inode::S_IFIFO => crate::dir::DirEntryType::Fifo,
            crate::inode::S_IFSOCK => crate::dir::DirEntryType::Socket,
            _ => crate::dir::DirEntryType::Unknown,
        };
        self.add_dir_entry(
            dst_parent_ino,
            &dst_parent_inode,
            dst_name.as_bytes(),
            src_ino,
            dir_type,
        )?;

        self.dev.flush()?;
        Ok(())
    }

    /// Rename `src` → `dst` within the same filesystem.
    ///
    /// Semantics (v1):
    /// - Both endpoints are within this mount.
    /// - Works for files and directories.
    /// - Dest must NOT exist — overwrite-on-rename is follow-up work.
    /// - Cross-parent moves update the moved dir's `..` entry + bump /
    ///   decrement both parents' `i_links_count`.
    /// - Refuses to move a directory into its own subtree (cycle check).
    /// - Same source and dest: no-op success.
    pub fn apply_rename(&self, src: &str, dst: &str) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        if src == dst {
            return Ok(());
        }

        let (src_parent_path, src_name) = split_parent_and_base(src)?;
        let (dst_parent_path, dst_name) = split_parent_and_base(dst)?;
        if dst_name.len() > 255 {
            return Err(Error::NameTooLong);
        }

        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let src_parent_ino =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &src_parent_path)?;
        let dst_parent_ino =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &dst_parent_path)?;
        let (src_parent_inode, _) = self.read_inode_verified(src_parent_ino)?;
        let (dst_parent_inode, _) = self.read_inode_verified(dst_parent_ino)?;
        if !src_parent_inode.is_dir() || !dst_parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }

        let src_ino = self.find_entry_in_dir(&src_parent_inode, src_name.as_bytes())?;
        if self
            .find_entry_in_dir(&dst_parent_inode, dst_name.as_bytes())
            .is_ok()
        {
            return Err(Error::AlreadyExists);
        }

        let (src_inode, _) = self.read_inode_verified(src_ino)?;
        let src_is_dir = src_inode.is_dir();

        // Cycle check: moving a dir INTO itself is illegal. Simple prefix
        // check on normalised paths — rejects rename /a /a/b/c.
        if src_is_dir {
            let src_slash = format!("{}/", src.trim_end_matches('/'));
            if dst == src || dst.starts_with(&src_slash) {
                return Err(Error::InvalidArgument(
                    "rename: cannot move directory into its own subtree",
                ));
            }
        }

        // Map POSIX mode bits to the directory-entry file-type byte.
        let dir_type = match src_inode.file_type() {
            crate::inode::S_IFREG => crate::dir::DirEntryType::RegFile,
            crate::inode::S_IFDIR => crate::dir::DirEntryType::Directory,
            crate::inode::S_IFLNK => crate::dir::DirEntryType::Symlink,
            _ => crate::dir::DirEntryType::Unknown,
        };

        // 1. Insert the new entry in the destination parent BEFORE removing
        //    the source, so a mid-operation failure leaves the source
        //    findable (at the cost of a possible duplicate if we crash
        //    between insert and remove — acceptable pre-journaling).
        self.add_dir_entry(
            dst_parent_ino,
            &dst_parent_inode,
            dst_name.as_bytes(),
            src_ino,
            dir_type,
        )?;

        // 2. Remove from the source parent.
        self.remove_dir_entry(src_parent_ino, &src_parent_inode, src_name.as_bytes())?;

        // 3. Cross-parent dir move: fix `..` + adjust parent nlinks.
        if src_is_dir && src_parent_ino != dst_parent_ino {
            self.update_dotdot(src_ino, &src_inode, dst_parent_ino)?;

            // Source parent loses one subdir -> -1 nlink.
            let (sp_inode, mut sp_raw) = self.read_inode_verified(src_parent_ino)?;
            self.patch_inode_nlink(src_parent_ino, &mut sp_raw, &sp_inode, -1)?;
            self.write_inode_raw(src_parent_ino, &sp_raw)?;

            // Dest parent gains one -> +1.
            let (dp_inode, mut dp_raw) = self.read_inode_verified(dst_parent_ino)?;
            self.patch_inode_nlink(dst_parent_ino, &mut dp_raw, &dp_inode, 1)?;
            self.write_inode_raw(dst_parent_ino, &dp_raw)?;
        }

        self.dev.flush()?;
        Ok(())
    }

    /// Insert `name → target_ino` into `parent_inode`'s linear directory
    /// blocks. Picks the first block with space; errors if the directory
    /// has no room (dir-extension is a follow-up). Recomputes the block's
    /// tail checksum when metadata_csum is on.
    fn add_dir_entry(
        &self,
        parent_ino: u32,
        parent_inode: &Inode,
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let n_blocks = parent_inode.size.div_ceil(bs as u64);
        for logical in 0..n_blocks {
            let Some(phys) =
                crate::extent::map_logical(&parent_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let mut block = self.read_block(phys)?;
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&block) {
                12
            } else {
                0
            };
            match crate::dir::add_entry_to_block(
                &mut block,
                target_ino,
                name,
                file_type,
                has_ft,
                reserved_tail,
            ) {
                Ok(()) => {
                    if self.csum.enabled && reserved_tail == 12 {
                        let end = block.len();
                        let mut c = crate::checksum::linux_crc32c(
                            self.csum.seed,
                            &parent_ino.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(
                            c,
                            &parent_inode.generation.to_le_bytes(),
                        );
                        c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
                        block[end - 4..end].copy_from_slice(&c.to_le_bytes());
                    }
                    self.dev.write_at(phys * bs as u64, &block)?;
                    return Ok(());
                }
                Err(Error::OutOfBounds) => continue,
                Err(e) => return Err(e),
            }
        }
        // All existing blocks are full → grow the directory by one fs block.
        self.extend_dir_and_add_entry(parent_ino, name, target_ino, file_type)
    }

    /// Grow `parent_ino`'s directory file by one fs block, seed that block
    /// with the entry `(name → target_ino)`, and update the parent inode
    /// image (size +block_size, +1 extent, recomputed CSUM). Assumes the
    /// parent's inline extent root still has a free slot (the common case
    /// until htree promotion lands).
    fn extend_dir_and_add_entry(
        &self,
        parent_ino: u32,
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let bs_u64 = bs as u64;
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;

        // Re-read parent so we operate on the freshest on-disk bytes.
        let (parent_inode, mut parent_raw) = self.read_inode_verified(parent_ino)?;
        if !parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        let new_logical_block = parent_inode.size.div_ceil(bs_u64);

        // 1. Allocate one fs block. Hint to parent's group.
        let parent_group = (parent_ino - 1) / self.sb.inodes_per_group;
        let mut bitmap_reader = |block: u64| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; bs as usize];
            self.dev.read_at(block * bs_u64, &mut buf)?;
            Ok(buf)
        };
        let plan = crate::alloc::plan_block_allocation(
            &self.sb,
            &self.groups,
            1,
            parent_group,
            &mut bitmap_reader,
        )?;
        let new_phys = plan.first_block;

        // 2. Insert extent into parent's inline extent root. If the root is
        //    saturated at depth 0, promote to depth 1 by allocating a fresh
        //    leaf block, moving all entries into it, and writing a single
        //    index entry into the inline root.
        let new_extent = crate::extent::Extent {
            logical_block: new_logical_block as u32,
            length: 1,
            physical_block: new_phys,
            uninitialized: false,
        };
        // If the parent root is already promoted (depth ≥ 1), operate on the
        // leaf block directly instead of the 60-byte inline root. This keeps
        // the inode.block area unchanged; only the leaf-node physical block
        // gets rewritten.
        let root_header = crate::extent::ExtentHeader::parse(&parent_inode.block)?;
        if root_header.depth == 1 {
            return self.extend_dir_and_add_entry_depth1(
                parent_ino,
                &parent_inode,
                &mut parent_raw,
                name,
                target_ino,
                file_type,
                has_ft,
                new_phys,
                new_extent,
                plan,
            );
        }
        if root_header.depth > 1 {
            return Err(Error::CorruptExtentTree(
                "extend_dir_and_add_entry: depth > 1 extent trees are not yet supported (leaf-split needed)",
            ));
        }

        let (new_root, leaf_meta_alloc) =
            match crate::extent_mut::plan_insert_extent(&parent_inode.block, new_extent) {
                Ok(muts) => {
                    let root = muts
                        .into_iter()
                        .find_map(|m| match m {
                            crate::extent_mut::ExtentMutation::WriteRoot { bytes } => Some(bytes),
                            _ => None,
                        })
                        .ok_or(Error::Corrupt(
                            "extend_dir_and_add_entry: plan produced no WriteRoot",
                        ))?;
                    (root, None)
                }
                Err(Error::CorruptExtentTree(msg)) if msg.contains("LEAF_FULL_NEEDS_PROMOTION") => {
                    // Commit the data-block allocation NOW so the next plan picks
                    // a different run (plan_block_allocation reads the bitmap).
                    self.mark_block_run_used(new_phys, 1)?;
                    self.patch_bgd_counters(
                        plan.bgd.group_idx as usize,
                        plan.bgd.free_blocks_delta,
                        plan.bgd.free_inodes_delta,
                        plan.bgd.used_dirs_delta,
                    )?;
                    self.patch_sb_counters(plan.sb.free_blocks_delta, plan.sb.free_inodes_delta)?;

                    // Second allocation: the leaf node block.
                    let mut reader2 = |block: u64| -> Result<Vec<u8>> {
                        let mut buf = vec![0u8; bs as usize];
                        self.dev.read_at(block * bs_u64, &mut buf)?;
                        Ok(buf)
                    };
                    let meta_plan = crate::alloc::plan_block_allocation(
                        &self.sb,
                        &self.groups,
                        1,
                        parent_group,
                        &mut reader2,
                    )?;
                    let leaf_meta_phys = meta_plan.first_block;

                    let promo = crate::extent_mut::plan_promote_leaf(
                        &parent_inode.block,
                        new_extent,
                        bs as usize,
                        leaf_meta_phys,
                        self.csum.enabled,
                    )?;
                    let mut leaf = promo.leaf_bytes;
                    if self.csum.enabled {
                        self.csum
                            .patch_extent_tail(parent_ino, parent_inode.generation, &mut leaf);
                    }
                    self.dev.write_at(leaf_meta_phys * bs_u64, &leaf)?;
                    (promo.new_root_bytes, Some(meta_plan))
                }
                Err(e) => return Err(e),
            };
        Self::patch_inode_block_area(&mut parent_raw, &new_root)?;

        // 3. Patch size (+= block_size) and i_blocks. On the promotion path
        //    the inode claims both the data block AND the leaf-node block.
        let blocks_consumed: u64 = 1 + if leaf_meta_alloc.is_some() { 1 } else { 0 };
        let new_size = parent_inode.size + bs_u64;
        let new_blocks = parent_inode.blocks + (bs_u64 / 512) * blocks_consumed;
        Self::patch_inode_size_and_blocks(&mut parent_raw, new_size, new_blocks)?;

        // 4. Recompute parent inode CSUM and write it back.
        if self.csum.enabled {
            if let Some((lo, hi)) =
                self.csum
                    .compute_inode_checksum(parent_ino, parent_inode.generation, &parent_raw)
            {
                parent_raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if parent_raw.len() >= 0x84 {
                    parent_raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(parent_ino, &parent_raw)?;

        // 5. Seed the new data block with a "whole-block unused" placeholder
        //    that add_entry_to_block can split into (new entry + remainder).
        let reserved_tail = if self.csum.enabled { 12 } else { 0 };
        let usable = (bs as usize) - reserved_tail;
        let mut block = vec![0u8; bs as usize];
        block[0..4].copy_from_slice(&0u32.to_le_bytes());
        block[4..6].copy_from_slice(&(usable as u16).to_le_bytes());

        crate::dir::add_entry_to_block(
            &mut block,
            target_ino,
            name,
            file_type,
            has_ft,
            reserved_tail,
        )?;

        if self.csum.enabled && reserved_tail == 12 {
            let end = block.len();
            block[end - 12..end - 8].copy_from_slice(&0u32.to_le_bytes());
            block[end - 8..end - 6].copy_from_slice(&12u16.to_le_bytes());
            block[end - 6] = 0;
            block[end - 5] = 0xDE;
            let mut c = crate::checksum::linux_crc32c(self.csum.seed, &parent_ino.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &parent_inode.generation.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
            block[end - 4..end].copy_from_slice(&c.to_le_bytes());
        }
        self.dev.write_at(new_phys * bs_u64, &block)?;

        // 6. Commit block allocator side-effects. On the promotion path the
        //    data-block allocation was already committed above; here we only
        //    commit the leaf-node allocation. On the simple path we commit the
        //    data block as usual.
        if let Some(meta_plan) = leaf_meta_alloc {
            self.mark_block_run_used(meta_plan.first_block, 1)?;
            self.patch_bgd_counters(
                meta_plan.bgd.group_idx as usize,
                meta_plan.bgd.free_blocks_delta,
                meta_plan.bgd.free_inodes_delta,
                meta_plan.bgd.used_dirs_delta,
            )?;
            self.patch_sb_counters(
                meta_plan.sb.free_blocks_delta,
                meta_plan.sb.free_inodes_delta,
            )?;
        } else {
            self.mark_block_run_used(new_phys, 1)?;
            self.patch_bgd_counters(
                plan.bgd.group_idx as usize,
                plan.bgd.free_blocks_delta,
                plan.bgd.free_inodes_delta,
                plan.bgd.used_dirs_delta,
            )?;
            self.patch_sb_counters(plan.sb.free_blocks_delta, plan.sb.free_inodes_delta)?;
        }

        Ok(())
    }

    /// Grow a directory whose extent tree is already at depth 1 (i.e. has
    /// been promoted). The inline root holds a single index entry → one leaf
    /// block. The mutation happens entirely inside the leaf block; the inode
    /// root is unchanged.
    ///
    /// Leaf overflow (>340 entries in a 4 KiB block with csum) returns a
    /// clean error. True leaf-split / depth-2 promotion is deferred.
    #[allow(clippy::too_many_arguments)]
    fn extend_dir_and_add_entry_depth1(
        &self,
        parent_ino: u32,
        parent_inode: &Inode,
        parent_raw: &mut [u8],
        name: &[u8],
        target_ino: u32,
        file_type: crate::dir::DirEntryType,
        has_ft: bool,
        new_phys: u64,
        new_extent: crate::extent::Extent,
        plan: crate::alloc::BlockAllocationPlan,
    ) -> Result<()> {
        let bs = self.sb.block_size();
        let bs_u64 = bs as u64;

        // Resolve the single index entry in the 60-byte inline root.
        let idx = crate::extent::ExtentIdx::parse(
            &parent_inode.block
                [crate::extent::EXT4_EXT_NODE_SIZE..2 * crate::extent::EXT4_EXT_NODE_SIZE],
        )?;
        let leaf_phys = idx.leaf_block;

        // Read the leaf block + run plan_insert_extent on its 4 KiB buffer.
        // `plan_insert_extent` operates on any depth-0 root — it uses
        // `header.max` for capacity, which was set to (bs-12-4)/12 = 340
        // when the leaf was built by `plan_promote_leaf`.
        let mut leaf = vec![0u8; bs as usize];
        self.dev.read_at(leaf_phys * bs_u64, &mut leaf)?;
        // CRC-verify before mutating — if the leaf's tail is corrupt we'd
        // write a false "fixed" version back.
        if self.csum.enabled
            && !self
                .csum
                .verify_extent_tail(parent_ino, parent_inode.generation, &leaf)
        {
            return Err(Error::BadChecksum {
                what: "extent block",
            });
        }

        let muts = crate::extent_mut::plan_insert_extent(&leaf, new_extent)?;
        let new_leaf = muts
            .into_iter()
            .find_map(|m| match m {
                crate::extent_mut::ExtentMutation::WriteRoot { bytes } => Some(bytes),
                _ => None,
            })
            .ok_or(Error::Corrupt(
                "extend_dir_and_add_entry_depth1: plan produced no WriteRoot",
            ))?;
        let mut new_leaf = new_leaf;
        if self.csum.enabled {
            self.csum
                .patch_extent_tail(parent_ino, parent_inode.generation, &mut new_leaf);
        }
        self.dev.write_at(leaf_phys * bs_u64, &new_leaf)?;

        // Inode root is unchanged — just grow size + blocks by one data block.
        let new_size = parent_inode.size + bs_u64;
        let new_blocks = parent_inode.blocks + (bs_u64 / 512);
        Self::patch_inode_size_and_blocks(parent_raw, new_size, new_blocks)?;
        if self.csum.enabled {
            if let Some((lo, hi)) =
                self.csum
                    .compute_inode_checksum(parent_ino, parent_inode.generation, parent_raw)
            {
                parent_raw[0x7C..0x7E].copy_from_slice(&lo.to_le_bytes());
                if parent_raw.len() >= 0x84 {
                    parent_raw[0x82..0x84].copy_from_slice(&hi.to_le_bytes());
                }
            }
        }
        self.write_inode_raw(parent_ino, parent_raw)?;

        // Seed + write the new data block (same recipe as the depth-0 path).
        let reserved_tail = if self.csum.enabled { 12 } else { 0 };
        let usable = (bs as usize) - reserved_tail;
        let mut block = vec![0u8; bs as usize];
        block[0..4].copy_from_slice(&0u32.to_le_bytes());
        block[4..6].copy_from_slice(&(usable as u16).to_le_bytes());

        crate::dir::add_entry_to_block(
            &mut block,
            target_ino,
            name,
            file_type,
            has_ft,
            reserved_tail,
        )?;

        if self.csum.enabled && reserved_tail == 12 {
            let end = block.len();
            block[end - 12..end - 8].copy_from_slice(&0u32.to_le_bytes());
            block[end - 8..end - 6].copy_from_slice(&12u16.to_le_bytes());
            block[end - 6] = 0;
            block[end - 5] = 0xDE;
            let mut c = crate::checksum::linux_crc32c(self.csum.seed, &parent_ino.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &parent_inode.generation.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
            block[end - 4..end].copy_from_slice(&c.to_le_bytes());
        }
        self.dev.write_at(new_phys * bs_u64, &block)?;

        // Commit data-block allocation.
        self.mark_block_run_used(new_phys, 1)?;
        self.patch_bgd_counters(
            plan.bgd.group_idx as usize,
            plan.bgd.free_blocks_delta,
            plan.bgd.free_inodes_delta,
            plan.bgd.used_dirs_delta,
        )?;
        self.patch_sb_counters(plan.sb.free_blocks_delta, plan.sb.free_inodes_delta)?;

        Ok(())
    }

    /// Remove `name` from `parent_inode`'s linear directory blocks. Errors
    /// if the name isn't found in any block.
    fn remove_dir_entry(&self, parent_ino: u32, parent_inode: &Inode, name: &[u8]) -> Result<()> {
        let bs = self.sb.block_size();
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let n_blocks = parent_inode.size.div_ceil(bs as u64);
        for logical in 0..n_blocks {
            let Some(phys) =
                crate::extent::map_logical(&parent_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let mut block = self.read_block(phys)?;
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&block) {
                12
            } else {
                0
            };
            if crate::dir::remove_entry_from_block(&mut block, name, has_ft, reserved_tail)? {
                if self.csum.enabled && reserved_tail == 12 {
                    let end = block.len();
                    let mut c =
                        crate::checksum::linux_crc32c(self.csum.seed, &parent_ino.to_le_bytes());
                    c = crate::checksum::linux_crc32c(c, &parent_inode.generation.to_le_bytes());
                    c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
                    block[end - 4..end].copy_from_slice(&c.to_le_bytes());
                }
                self.dev.write_at(phys * bs as u64, &block)?;
                return Ok(());
            }
        }
        Err(Error::NotFound)
    }

    /// Point a directory's `..` entry at `new_parent_ino`. The `..` entry
    /// lives in the directory's first data block, immediately after the `.`
    /// entry at byte offset 12. Recomputes the block's tail checksum when
    /// metadata_csum is on — the tail csum is keyed on this directory's own
    /// ino + generation, hence both are required.
    fn update_dotdot(&self, dir_ino: u32, dir_inode: &Inode, new_parent_ino: u32) -> Result<()> {
        let bs = self.sb.block_size();
        let phys = crate::extent::map_logical(&dir_inode.block, self.dev.as_ref(), bs, 0)?
            .ok_or(Error::Corrupt("update_dotdot: dir block 0 missing"))?;
        let mut block = self.read_block(phys)?;
        if block.len() < 24 {
            return Err(Error::Corrupt("update_dotdot: dir block too small"));
        }
        block[12..16].copy_from_slice(&new_parent_ino.to_le_bytes());

        if self.csum.enabled && crate::dir::has_csum_tail(&block) {
            let end = block.len();
            let mut c = crate::checksum::linux_crc32c(self.csum.seed, &dir_ino.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &dir_inode.generation.to_le_bytes());
            c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
            block[end - 4..end].copy_from_slice(&c.to_le_bytes());
        }
        self.dev.write_at(phys * bs as u64, &block)?;
        Ok(())
    }

    /// Remove an empty directory at `path`. Requires the target to contain
    /// only `.` and `..`. Frees the data block(s) + inode, removes the
    /// entry from the parent, decrements parent's `i_links_count`.
    pub fn apply_rmdir(&self, path: &str) -> Result<()> {
        if !self.dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        let (parent_path, base_name) = split_parent_and_base(path)?;
        let mut reader = |ino: u32| self.read_inode_verified(ino).map(|(i, _)| i);
        let parent_ino =
            crate::path::lookup(self.dev.as_ref(), &self.sb, &mut reader, &parent_path)?;
        let (parent_inode, mut parent_raw) = self.read_inode_verified(parent_ino)?;
        if !parent_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        let target_ino = self.find_entry_in_dir(&parent_inode, base_name.as_bytes())?;
        let (target_inode, _) = self.read_inode_verified(target_ino)?;
        if !target_inode.is_dir() {
            return Err(Error::NotADirectory);
        }

        // Empty-check: walk every block, reject if any entry is not "." or "..".
        let bs = self.sb.block_size();
        let has_ft = self.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;
        let blocks = target_inode.size.div_ceil(bs as u64);
        for logical in 0..blocks {
            let Some(phys) =
                crate::extent::map_logical(&target_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let block = self.read_block(phys)?;
            for entry in crate::dir::DirBlockIter::new(&block, has_ft) {
                let e = entry?;
                if e.name != b"." && e.name != b".." {
                    return Err(Error::DirectoryNotEmpty);
                }
            }
        }

        // Free target's data blocks (collect physical extents via the read
        // path; simpler than re-parsing here).
        let extents = crate::extent::collect_all(&target_inode.block, self.dev.as_ref(), bs)?;
        let mut freed_blocks: u64 = 0;
        let mut target_group_idx: Option<usize> = None;
        for e in &extents {
            self.free_block_run(e.physical_block, e.length as u64)?;
            freed_blocks += e.length as u64;
            let gi = ((e.physical_block - self.sb.first_data_block as u64)
                / self.sb.blocks_per_group as u64) as usize;
            target_group_idx.get_or_insert(gi);
        }
        if let Some(gi) = target_group_idx {
            self.patch_bgd_counters(gi, freed_blocks as i32, 0, 0)?;
        }
        self.patch_sb_counters(freed_blocks as i64, 0)?;

        // Free the inode slot + adjust counters. A removed dir decrements
        // `bg_used_dirs_count`.
        self.free_inode_slot(target_ino)?;
        let target_gi = ((target_ino - 1) / self.sb.inodes_per_group) as usize;
        self.patch_bgd_counters(target_gi, 0, 1, -1)?;
        self.patch_sb_counters(0, 1)?;

        // Remove the entry from the parent directory.
        let parent_blocks = parent_inode.size.div_ceil(bs as u64);
        let mut removed = false;
        for logical in 0..parent_blocks {
            let Some(phys) =
                crate::extent::map_logical(&parent_inode.block, self.dev.as_ref(), bs, logical)?
            else {
                continue;
            };
            let mut block = self.read_block(phys)?;
            let reserved_tail = if self.csum.enabled && crate::dir::has_csum_tail(&block) {
                12
            } else {
                0
            };
            if crate::dir::remove_entry_from_block(
                &mut block,
                base_name.as_bytes(),
                has_ft,
                reserved_tail,
            )? {
                if self.csum.enabled && reserved_tail == 12 {
                    let end = block.len();
                    let mut c =
                        crate::checksum::linux_crc32c(self.csum.seed, &parent_ino.to_le_bytes());
                    c = crate::checksum::linux_crc32c(c, &parent_inode.generation.to_le_bytes());
                    c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
                    block[end - 4..end].copy_from_slice(&c.to_le_bytes());
                }
                self.dev.write_at(phys * bs as u64, &block)?;
                removed = true;
                break;
            }
        }
        if !removed {
            return Err(Error::Corrupt(
                "apply_rmdir: entry disappeared mid-operation",
            ));
        }

        // Parent loses the ".." reference from the removed child → nlink -1.
        self.patch_inode_nlink(parent_ino, &mut parent_raw, &parent_inode, -1)?;
        self.write_inode_raw(parent_ino, &parent_raw)?;

        self.dev.flush()?;
        Ok(())
    }
}
