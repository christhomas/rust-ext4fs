//! Regression: a group's INODE_UNINIT / BLOCK_UNINIT must be cleared the
//! moment something is really allocated out of that group.
//!
//! Those flags are every reader's licence to ignore what is physically in a
//! group's bitmap blocks and treat the whole group as free — that is exactly
//! why mkfs is allowed to leave those blocks holding unspecified bytes. Once a
//! real inode or block is handed out of such a group, the licence has to be
//! revoked in the same transaction. Leave it standing and the *next* mount
//! still believes the group is empty, hands out the very same inode and block
//! numbers again, and silently writes over what was just stored there.
//!
//! Orlov directory spreading is what makes this reachable in ordinary use: a
//! new directory is deliberately placed away from its parent's group, which on
//! a freshly formatted volume means a group nothing has touched yet — and on a
//! freshly formatted volume every such group is still uninit.
//!
//! The device is deliberately formatted with 2 KiB blocks — the smallest
//! `mkfs` accepts for a multi-group volume — so that `blocks_per_group`
//! (8 × block size) is 16384, i.e. 32 MiB per group. That fits four groups
//! into a 128 MiB image the test can hold in memory, where 4 KiB blocks
//! would need 128 MiB per group and so four times the memory.

use fs_ext4::bgd::BgdFlags;
use fs_ext4::block_io::BlockDevice;
use fs_ext4::error::Result;
use fs_ext4::file_io;
use fs_ext4::fs::Filesystem;
use fs_ext4::inode::Inode;
use fs_ext4::mkfs;
use fs_ext4::path as path_mod;
use std::sync::{Arc, Mutex};

const BLOCK_SIZE: u32 = 2048;
const IMAGE_BYTES: u64 = 128 * 1024 * 1024;

/// In-memory read/write block device, so the test needs no fixture on disk
/// and can be re-mounted from the same bytes.
struct MemDev {
    bytes: Mutex<Vec<u8>>,
    size: u64,
}

impl MemDev {
    fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(vec![0u8; size as usize]),
            size,
        })
    }
}

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        assert!(end <= b.len(), "read past EOF");
        buf.copy_from_slice(&b[start..end]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        assert!(end <= b.len(), "write past EOF");
        b[start..end].copy_from_slice(buf);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Put group `gi` into the state a real `mkfs.ext4` leaves an untouched group
/// in, which this crate's own `mkfs` never produces: both uninit bits set, and
/// bitmap blocks holding bytes no reader is permitted to look at.
///
/// The 0xFF fill is the interesting half. It is not arbitrary corruption — it
/// is the *worst legal* content for a block the flags say to ignore, so a
/// reader that honours the flag sees an empty group while a reader that peeks
/// sees a full one. That difference is what makes "did we remember to zero the
/// bitmap when we took the flag down" observable at all.
///
/// The descriptor checksum has to be recomputed, or the next mount rejects the
/// group before any of this is reached.
fn mark_group_uninit(dev: &MemDev, fs: &Filesystem, gi: usize) {
    const INODE_UNINIT: u16 = 0x0001;
    const BLOCK_UNINIT: u16 = 0x0002;

    let bs = fs.sb.block_size() as u64;
    let garbage = vec![0xFFu8; bs as usize];
    for block in [fs.groups[gi].block_bitmap, fs.groups[gi].inode_bitmap] {
        dev.write_at(block * bs, &garbage).expect("scribble bitmap");
    }

    let desc_size = fs.sb.desc_size as usize;
    let byte_in_bgt = gi as u64 * desc_size as u64;
    let bgt_block = fs.sb.first_data_block as u64 + 1 + byte_in_bgt / bs;
    let desc_at = bgt_block * bs + byte_in_bgt % bs;

    let mut desc = vec![0u8; desc_size];
    dev.read_at(desc_at, &mut desc).expect("read bgd");

    let flags = u16::from_le_bytes(desc[0x12..0x14].try_into().unwrap());
    desc[0x12..0x14].copy_from_slice(&(flags | INODE_UNINIT | BLOCK_UNINIT).to_le_bytes());

    if fs.csum.enabled {
        // Same construction as the driver's own BGD writes: seed, then the
        // group number, then the descriptor with its checksum field zeroed.
        desc[0x1E..0x20].copy_from_slice(&[0, 0]);
        let c = fs_ext4::checksum::linux_crc32c(fs.csum.seed, &(gi as u32).to_le_bytes());
        let c = fs_ext4::checksum::linux_crc32c(c, &desc);
        desc[0x1E..0x20].copy_from_slice(&(c as u16).to_le_bytes());
    }
    dev.write_at(desc_at, &desc).expect("write bgd");
}

/// A formatted image whose later groups look the way `mkfs.ext4` would have
/// left them. Returns the device and the groups that were made uninit.
fn device_with_uninit_groups() -> (Arc<MemDev>, Vec<usize>) {
    let dev = MemDev::new(IMAGE_BYTES);
    mkfs::format_filesystem(dev.as_ref(), Some("UNINIT"), None, IMAGE_BYTES, BLOCK_SIZE)
        .expect("format");

    let fs = Filesystem::mount(dev.clone()).expect("mount to locate groups");
    assert!(
        fs.groups.len() > 1,
        "image must span several groups to place a directory away from root's group"
    );
    // Group 0 holds the root directory and is genuinely initialised; every
    // other group on a fresh volume is untouched and may be flagged uninit.
    let uninit: Vec<usize> = (1..fs.groups.len()).collect();
    for &gi in &uninit {
        mark_group_uninit(&dev, &fs, gi);
    }
    (dev, uninit)
}

fn resolve(fs: &Filesystem, path: &str) -> u32 {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    path_mod::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).expect("resolve")
}

fn read_all(fs: &Filesystem, path: &str) -> Vec<u8> {
    let ino = resolve(fs, path);
    let (inode, _) = fs.read_inode_verified(ino).expect("read inode");
    let mut buf = vec![0u8; inode.size as usize];
    file_io::read(fs, &inode, 0, inode.size, &mut buf).expect("read");
    buf
}

/// Which block group inode `ino` lives in.
fn group_of_inode(fs: &Filesystem, ino: u32) -> usize {
    ((ino - 1) / fs.sb.inodes_per_group) as usize
}

/// The blocks an inode actually occupies, as a flat list of physical block
/// numbers — enough to tell "these two files share a block" from "they don't".
fn mapped_blocks(fs: &Filesystem, inode: &Inode) -> Vec<u64> {
    let bs = fs.sb.block_size() as u64;
    let nblocks = inode.size.div_ceil(bs);
    (0..nblocks)
        .filter_map(|lb| fs.map_inode_logical(inode, lb).ok().flatten())
        .collect()
}

/// The whole point, end to end: allocate into a still-uninit group, remount,
/// allocate again, and the second allocation must not land on top of the first.
///
/// Without the flag being cleared the remount re-reads the group as empty and
/// re-issues the same inode number, so `first` and `second` come back equal and
/// the first file's contents are gone.
#[test]
fn allocation_in_an_uninit_group_survives_a_remount() {
    let (dev, uninit_groups) = device_with_uninit_groups();

    // Confirm the premise before relying on it: the groups really do read back
    // as uninit, and the descriptors still verify after being rewritten.
    {
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        for &gi in &uninit_groups {
            assert!(
                fs.groups[gi].flags().contains(BgdFlags::INODE_UNINIT),
                "group {gi} should read back as INODE_UNINIT"
            );
        }
    }

    let first_content = b"first file, written before the remount\n";
    let (first_ino, first_blocks) = {
        let fs = Filesystem::mount(dev.clone()).expect("mount rw");
        // Orlov puts a new directory in a group other than its parent's, which
        // on a fresh volume is one of the untouched, still-uninit groups.
        let dir_ino = fs.apply_mkdir("/spread", 0o755).expect("mkdir");
        assert!(
            uninit_groups.contains(&group_of_inode(&fs, dir_ino)),
            "sanity: the new directory should have landed in a previously-uninit group"
        );

        let first_ino = fs.apply_create("/spread/first.txt", 0o644).expect("create");
        fs.apply_pwrite("/spread/first.txt", 0, first_content)
            .expect("pwrite");

        let (inode, _) = fs.read_inode_verified(first_ino).expect("read inode");
        (first_ino, mapped_blocks(&fs, &inode))
    };

    // Remount from the very same bytes. This is where the bug bit: the second
    // mount re-reads the BGDs, still sees the group flagged uninit, and treats
    // every inode and block in it as free again.
    let second_content = b"second file, written after the remount\n";
    let fs = Filesystem::mount(dev.clone()).expect("remount");
    let second_ino = fs
        .apply_create("/spread/second.txt", 0o644)
        .expect("create after remount");
    fs.apply_pwrite("/spread/second.txt", 0, second_content)
        .expect("pwrite after remount");

    assert_ne!(
        first_ino, second_ino,
        "remount re-issued inode {first_ino}: the group's INODE_UNINIT was never cleared, \
         so the second mount believed the whole group was still free"
    );

    let (second_inode, _) = fs.read_inode_verified(second_ino).expect("read inode");
    let second_blocks = mapped_blocks(&fs, &second_inode);
    for b in &second_blocks {
        assert!(
            !first_blocks.contains(b),
            "block {b} was handed to both files: the group's BLOCK_UNINIT was never cleared"
        );
    }

    // The observable damage the flags were protecting against.
    assert_eq!(
        read_all(&fs, "/spread/first.txt"),
        first_content,
        "the first file's contents were overwritten by the second allocation"
    );
    assert_eq!(read_all(&fs, "/spread/second.txt"), second_content);
}

/// The flags themselves, checked directly: after allocating out of a group,
/// neither uninit bit may still be set on it.
///
/// The end-to-end test above is the one that matters, but it can only fail
/// once the damage is already observable. This one names the cause.
#[test]
fn allocating_from_a_group_clears_its_uninit_flags() {
    let (dev, _uninit_groups) = device_with_uninit_groups();

    let touched = {
        let fs = Filesystem::mount(dev.clone()).expect("mount rw");
        let dir_ino = fs.apply_mkdir("/spread", 0o755).expect("mkdir");
        let file_ino = fs.apply_create("/spread/f.txt", 0o644).expect("create");
        // Enough bytes to force a real block allocation, not just an inode.
        fs.apply_pwrite("/spread/f.txt", 0, &[0xab; 4096])
            .expect("pwrite");
        vec![group_of_inode(&fs, dir_ino), group_of_inode(&fs, file_ino)]
    };

    // Re-read the descriptors from disk rather than trusting the in-memory
    // copy of the filesystem that just wrote them.
    let fs = Filesystem::mount(dev).expect("remount");
    for gi in touched {
        let flags = fs.groups[gi].flags();
        assert!(
            !flags.contains(BgdFlags::INODE_UNINIT),
            "group {gi} still claims INODE_UNINIT after an inode was allocated from it"
        );
        assert!(
            !flags.contains(BgdFlags::BLOCK_UNINIT),
            "group {gi} still claims BLOCK_UNINIT after a block was allocated from it"
        );
    }
}
