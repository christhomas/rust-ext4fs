//! Every directory block this driver writes must carry a valid tail
//! checksum.
//!
//! # Why this file exists
//!
//! The `ext4_dir_entry_tail` recipe — plant a fake dirent
//! (`inode = 0`, `rec_len = 12`, `name_len = 0`, `file_type = 0xDE`),
//! then `crc32c(seed → ino → generation → block[..len - 12])` — is
//! hand-written in **sixteen places** across `fs.rs`, `fsck.rs`,
//! `mkfs.rs` and the test suite. Nothing forces them to agree.
//!
//! Before this file, nothing noticed when they did not. Corrupting the
//! CRC span at `seed_directory_block`, at the entry-add path, and at
//! all three `fsck.rs` repair sites left **every** test passing,
//! including the ones that verify checksums — because those verify the
//! blocks `mkfs` wrote, not the ones the driver writes afterwards.
//!
//! A wrong tail produces no error here. It surfaces when the volume is
//! mounted by Linux, which reports the directory as corrupt.
//!
//! # What it checks
//!
//! After each mutating operation, every directory block reachable from
//! the root is re-read and its tail verified against
//! `Checksummer::verify_dir_entry_tail` — the read-side function, which
//! is the one place the recipe is written down as a check rather than
//! as a write.

use fs_ext4::block_io::BlockDevice;
use fs_ext4::dir;
use fs_ext4::error::Result;
use fs_ext4::extent;
use fs_ext4::fs::Filesystem;
use fs_ext4::mkfs;
use std::sync::{Arc, Mutex};

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
        buf.copy_from_slice(&b[start..start + buf.len()]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A freshly formatted 64 MiB volume with `metadata_csum` on.
fn fresh() -> (Arc<MemDev>, Arc<dyn BlockDevice>) {
    let size: u64 = 64 * 1024 * 1024;
    let dev = MemDev::new(size);
    mkfs::format_filesystem(dev.as_ref(), Some("DIRCSUM"), Some([0x5A; 16]), size, 4096)
        .expect("format_filesystem");
    let dyn_dev: Arc<dyn BlockDevice> = dev.clone();
    (dev, dyn_dev)
}

/// Verify the tail of every directory block reachable from the root.
///
/// Walks breadth-first from inode 2, following every dirent whose
/// `file_type` says directory. Returns how many blocks it checked, so a
/// test can assert it actually looked at something — an assertion that
/// silently checks nothing is the failure mode this whole file is
/// about.
fn check_every_directory_block(
    fs: &Filesystem,
    dev: &Arc<dyn BlockDevice>,
    context: &str,
) -> usize {
    let bs = fs.sb.block_size();
    let has_ft = true;
    let mut queue = vec![2u32];
    let mut seen = vec![2u32];
    let mut checked = 0usize;

    while let Some(ino) = queue.pop() {
        let Ok((inode, _)) = fs.read_inode_verified(ino) else {
            continue;
        };
        if !inode.is_dir() || !inode.has_extents() {
            continue;
        }
        let blocks = inode.size.div_ceil(u64::from(bs));
        for logical in 0..blocks {
            let Ok(Some(phys)) = extent::map_logical(&inode.block, dev.as_ref(), bs, logical)
            else {
                continue;
            };
            let mut block = vec![0u8; bs as usize];
            dev.read_at(phys * u64::from(bs), &mut block)
                .expect("read dir block");
            if !dir::has_csum_tail(&block) {
                continue;
            }
            assert!(
                fs.csum.verify_dir_entry_tail(ino, inode.generation, &block),
                "{context}: dir inode {ino} logical block {logical} (physical {phys}) has a \
                 tail checksum that does not verify — a directory Linux will call corrupt"
            );
            checked += 1;

            for e in dir::parse_block(&block, has_ft).unwrap_or_default() {
                let is_dir = e.file_type == dir::DirEntryType::Directory;
                let is_dot = e.name == b"." || e.name == b"..";
                if is_dir && !is_dot && e.inode != 0 && !seen.contains(&e.inode) {
                    seen.push(e.inode);
                    queue.push(e.inode);
                }
            }
        }
    }
    checked
}

/// `apply_mkdir` seeds a brand-new directory block, tail and all.
///
/// This is `seed_directory_block`, which writes the recipe with a
/// different addressing idiom (`tail = bs - 12`, `tail + 4/+6/+7`) from
/// every other site (`end = block.len()`, `end - 8/-6/-5`). Nothing
/// checked that the two agree.
#[test]
fn a_new_directory_gets_a_valid_tail_checksum() {
    let (_dev, dyn_dev) = fresh();
    let fs = Filesystem::mount(dyn_dev.clone()).expect("mount");
    assert!(fs.csum.enabled, "the fixture must have metadata_csum on");

    fs.apply_mkdir("/alpha", 0o755).expect("mkdir /alpha");
    fs.apply_mkdir("/alpha/beta", 0o755).expect("mkdir nested");

    let checked = check_every_directory_block(&fs, &dyn_dev, "after mkdir");
    assert!(
        checked >= 3,
        "expected the root and two new directories, checked {checked}"
    );
}

/// Adding entries rewrites the parent's block; its tail must follow.
#[test]
fn adding_entries_keeps_the_parent_block_checksummed() {
    let (_dev, dyn_dev) = fresh();
    let fs = Filesystem::mount(dyn_dev.clone()).expect("mount");

    fs.apply_mkdir("/holder", 0o755).expect("mkdir");
    for i in 0..24 {
        fs.apply_create(&format!("/holder/file{i:03}.txt"), 0o644)
            .unwrap_or_else(|e| panic!("create file{i:03}: {e}"));
    }

    let checked = check_every_directory_block(&fs, &dyn_dev, "after creates");
    assert!(checked >= 2, "checked {checked}");
}

/// Removing entries rewrites the block too.
#[test]
fn removing_entries_keeps_the_parent_block_checksummed() {
    let (_dev, dyn_dev) = fresh();
    let fs = Filesystem::mount(dyn_dev.clone()).expect("mount");

    fs.apply_mkdir("/holder", 0o755).expect("mkdir");
    for i in 0..12 {
        fs.apply_create(&format!("/holder/f{i:02}"), 0o644)
            .expect("create");
    }
    for i in 0..12 {
        if i % 2 == 0 {
            fs.apply_unlink(&format!("/holder/f{i:02}"))
                .expect("unlink");
        }
    }

    let checked = check_every_directory_block(&fs, &dyn_dev, "after unlinks");
    assert!(checked >= 2, "checked {checked}");
}

/// Growing a directory past one block goes through
/// `extend_dir_and_add_entry`, which writes the freshly allocated
/// block's tail itself.
///
/// # The check has to happen on the create that grew the directory
///
/// Every later in-place add rewrites the same block and recomputes its
/// checksum at a *different* site. So a wrong checksum from the
/// extension path is overwritten by a right one, and a test that
/// creates two hundred files and then looks sees nothing — which is
/// exactly what the first version of this test did, and why corrupting
/// the extension path's CRC span left it passing.
///
/// This one stops the moment the directory grows, so the extension
/// path's write is the last thing to touch the new block.
#[test]
fn the_block_a_growing_directory_allocates_is_checksummed() {
    let (_dev, dyn_dev) = fresh();
    let fs = Filesystem::mount(dyn_dev.clone()).expect("mount");
    let bs = u64::from(fs.sb.block_size());

    fs.apply_mkdir("/big", 0o755).expect("mkdir");

    let dir_size = |fs: &Filesystem| -> u64 {
        let ino = fs_ext4::path::lookup(
            dyn_dev.as_ref(),
            &fs.sb,
            &mut |i| fs.read_inode_verified(i).map(|(x, _)| x),
            "/big",
        )
        .expect("look up /big");
        fs.read_inode_verified(ino).expect("read /big").0.size
    };

    let before = dir_size(&fs);
    assert_eq!(before, bs, "a new directory starts at one block");

    // Long names so a block fills in tens of entries rather than
    // hundreds: each dirent costs 8 + round_up(name_len, 4).
    let mut grew = false;
    for i in 0..400 {
        let name = format!("/big/a-deliberately-long-entry-name-number-{i:04}.dat");
        fs.apply_create(&name, 0o644)
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
        if dir_size(&fs) > before {
            grew = true;
            break;
        }
    }
    assert!(grew, "/big never grew past one block");

    let checked = check_every_directory_block(
        &fs,
        &dyn_dev,
        "immediately after the create that grew the directory",
    );
    assert!(checked >= 3, "checked {checked}");
}

/// A rename touches both parents' blocks, and a directory move
/// rewrites the moved directory's `..` as well.
#[test]
fn renaming_keeps_every_touched_block_checksummed() {
    let (_dev, dyn_dev) = fresh();
    let fs = Filesystem::mount(dyn_dev.clone()).expect("mount");

    fs.apply_mkdir("/from", 0o755).expect("mkdir from");
    fs.apply_mkdir("/to", 0o755).expect("mkdir to");
    fs.apply_mkdir("/from/sub", 0o755).expect("mkdir sub");
    fs.apply_create("/from/a.txt", 0o644).expect("create");

    fs.apply_rename("/from/a.txt", "/to/renamed-considerably-longer.txt", false)
        .expect("rename file across parents");
    fs.apply_rename("/from/sub", "/to/sub", false)
        .expect("rename directory across parents");

    let checked = check_every_directory_block(&fs, &dyn_dev, "after renames");
    assert!(checked >= 4, "checked {checked}");
}
