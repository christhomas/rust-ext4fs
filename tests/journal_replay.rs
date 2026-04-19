//! Journal replay integration test.
//!
//! Builds a "dirty" JBD2 journal scenario by hand on a copy of
//! ext4-basic.img: writes a synthetic descriptor + data + commit block
//! into the journal file, bumps `jsb.start`, and then verifies that
//! `journal_apply::replay_if_dirty` drives the buffered data block to its
//! destination fs block.
//!
//! We can't easily produce a real dirty journal on disk without either
//! (a) crashing a kernel mount or (b) reimplementing the full write path.
//! So this test exercises the walker → applier pipeline by hand-crafting
//! a minimally valid transaction that happens to not conflict with the
//! filesystem's actual data, then replaying.

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::jbd2::{self, JBD2_MAGIC_NUMBER};
use fs_ext4::journal;
use fs_ext4::journal_apply;
use fs_ext4::transaction::Transaction;
use fs_ext4::Filesystem;
use std::fs;
use std::sync::Arc;

fn image_path(name: &str) -> String {
    format!("{}/test-disks/{}", env!("CARGO_MANIFEST_DIR"), name)
}

fn copy_to_tmp(name: &str) -> Option<String> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let src = image_path(name);
    if !std::path::Path::new(&src).exists() {
        return None;
    }
    // Unique-per-call: cargo runs test fns in parallel threads; a shared name
    // would race on create/delete.
    let dst = format!(
        "/tmp/fs_ext4_replay_{}_{n}_{}.img",
        std::process::id(),
        name
    );
    fs::copy(&src, &dst).ok()?;
    Some(dst)
}

#[test]
fn writable_mount_preserves_read_path() {
    // A read-write-opened copy of ext4-basic.img must mount cleanly and
    // return the same sb info as a read-only mount.
    let Some(path) = copy_to_tmp("ext4-basic.img") else {
        return;
    };
    let dev = FileDevice::open_rw(&path).expect("open_rw");
    assert!(dev.is_writable());
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount rw");
    assert!(fs.sb.block_size() > 0);
    drop(fs);
    fs::remove_file(path).ok();
}

#[test]
fn clean_journal_is_no_op() {
    // A freshly-built image has jsb.start == 0 (clean). replay_if_dirty
    // must return 0 and not attempt any writes.
    let Some(path) = copy_to_tmp("ext4-basic.img") else {
        return;
    };
    let dev = FileDevice::open_rw(&path).expect("open_rw");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let n = journal_apply::replay_if_dirty(&fs).expect("replay_if_dirty");
    assert_eq!(n, 0, "clean journal should replay 0 blocks, got {n}");
    fs::remove_file(path).ok();
}

#[test]
fn synthetic_dirty_journal_round_trip() {
    // End-to-end: inject a descriptor+data+commit sequence into the journal
    // file, bump jsb.start so walk() sees it as dirty, replay, and assert
    // the target fs block now holds the data we wrote.
    let Some(path) = copy_to_tmp("ext4-basic.img") else {
        return;
    };

    let dev = Arc::new(FileDevice::open_rw(&path).expect("open_rw")) as Arc<dyn BlockDevice>;
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    let block_size = fs.sb.block_size() as u64;

    // Locate journal inode's physical blocks for logical 0..4 so we can
    // splice our synthetic transaction in.
    let raw = fs.read_inode_raw(fs.sb.journal_inode).expect("read jinode");
    let jinode = fs_ext4::inode::Inode::parse(&raw).expect("parse jinode");

    // Read the existing JBD2 superblock so we keep its features consistent.
    let jsb = jbd2::read_superblock(&fs)
        .expect("read jsb")
        .expect("journal present");

    // Pick an fs block well past the superblock + BGD area to act as our
    // replay target. For 4 KiB-block ext4-basic.img with small fs, block
    // 100 is safe (well inside free space).
    let target_fs_block: u64 = 100;
    let payload_byte: u8 = 0xA5;

    // Build a 1-entry transaction targeting block 100.
    let mut tx = Transaction::begin(
        jsb.sequence,
        block_size as u32,
        jsb.uses_64bit(),
        jsb.feature_incompat & fs_ext4::jbd2::JbdIncompat::CSUM_V3.bits() != 0,
    );
    let mut payload = vec![0u8; block_size as usize];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = payload_byte.wrapping_add((i & 0xFF) as u8);
    }
    tx.add_write(target_fs_block, payload.clone())
        .expect("add_write");
    let blocks = tx.commit().expect("commit");
    assert_eq!(blocks.len(), 3, "desc + data + commit");

    // Splice the three blocks into journal logical blocks 1, 2, 3
    // (logical 0 is the jsb itself; we leave it alone).
    for (i, blk) in blocks.iter().enumerate() {
        let journal_logical = (i as u64) + 1;
        let phys = jbd2::journal_block_to_physical(&fs, &jinode, journal_logical)
            .expect("map journal block")
            .expect("mapped");
        dev.write_at(phys * block_size, blk)
            .expect("write journal slot");
    }

    // Capture the target's pre-replay contents so we can assert the change.
    let mut before = vec![0u8; block_size as usize];
    dev.read_at(target_fs_block * block_size, &mut before)
        .unwrap();

    // Rewrite the JBD2 superblock with jsb.start = 1 (log starts at journal
    // logical block 1). Compose a minimal dirty sb by reading + patching.
    let jsb_phys = jbd2::journal_block_to_physical(&fs, &jinode, 0)
        .expect("jsb phys")
        .expect("mapped");
    let mut jsb_bytes = vec![0u8; block_size as usize];
    dev.read_at(jsb_phys * block_size, &mut jsb_bytes).unwrap();
    // Sanity: magic must match.
    let magic = u32::from_be_bytes(jsb_bytes[0..4].try_into().unwrap());
    assert_eq!(magic, JBD2_MAGIC_NUMBER);
    // Patch s_start (offset 0x1C..0x20, big-endian) to 1.
    jsb_bytes[0x1C..0x20].copy_from_slice(&1u32.to_be_bytes());
    dev.write_at(jsb_phys * block_size, &jsb_bytes).unwrap();
    dev.flush().unwrap();

    // Now re-mount so mount sees the dirty sb — actually we don't need to
    // remount: read_superblock fetches fresh every call.
    let n = journal_apply::replay_if_dirty(&fs).expect("replay");
    // We placed 1 write tag; filter_revoked runs but has no revokes — so 1 write.
    assert_eq!(n, 1, "expected 1 block replayed, got {n}");

    // Verify the target fs block now holds our payload.
    let mut after = vec![0u8; block_size as usize];
    dev.read_at(target_fs_block * block_size, &mut after)
        .unwrap();
    assert_eq!(after, payload, "replay did not write the expected payload");
    assert_ne!(
        before, after,
        "payload identical to pre-state — test is tautological"
    );

    drop(fs);
    fs::remove_file(path).ok();
}

#[test]
fn read_only_device_skips_replay_silently() {
    // A read-only open on a clean image: replay_if_dirty returns 0 without
    // error even though write_at would fail.
    let Some(path) = copy_to_tmp("ext4-basic.img") else {
        return;
    };
    let dev = FileDevice::open(&path).expect("open RO");
    assert!(!dev.is_writable());
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount RO");
    let n = journal_apply::replay_if_dirty(&fs).expect("replay_if_dirty");
    assert_eq!(n, 0);
    // Sanity-check: journal walk still works (no I/O to write path).
    if let Some(jsb) = jbd2::read_superblock(&fs).unwrap() {
        let _ = journal::walk(&fs, &jsb).unwrap_or_default();
    }
    fs::remove_file(path).ok();
}
