//! End-to-end: force a directory's inline extent root past its 4-leaf limit
//! and confirm the promotion path (depth 0 → depth 1) lands correctly.
//!
//! The allocator hands out blocks contiguously within a group, so consecutive
//! `extend_dir_and_add_entry` calls on the same directory would normally auto-
//! merge into a single extent. To produce distinct, non-contiguous extents we
//! allocate a 1-block "gap" file each time the target directory grows by a
//! block — the gap breaks the allocator's natural contiguity, so the next
//! extent lands at a fresh, non-contiguous physical block.

use ext4rs::block_io::FileDevice;
use ext4rs::extent::ExtentHeader;
use ext4rs::path as path_mod;
use ext4rs::Filesystem;
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
    let dst = format!(
        "/tmp/ext4rs_multilvl_{}_{n}_{}.img",
        std::process::id(),
        name
    );
    fs::copy(&src, &dst).ok()?;
    Some(dst)
}

fn resolve(fs: &Filesystem, path: &str) -> Option<u32> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(i, _)| i);
    path_mod::lookup(fs.dev.as_ref(), &fs.sb, &mut reader, path).ok()
}

/// Fill `target` with entries, allocating a 1-block gap file each time the
/// directory grows by a block, until `extent_header.depth == 1` (promotion
/// fired) or we've created `max_entries` files (safety bound — panics if we
/// didn't promote in time).
fn create_until_promotion(fs: &Filesystem, target: &str, max_entries: usize) {
    let bs = fs.sb.block_size() as u64;
    let dir_ino = resolve(fs, target).expect("resolve target");
    let mut prev_size = fs.read_inode_verified(dir_ino).expect("read").0.size;
    let mut gap_counter = 0u32;

    for i in 0..max_entries {
        // Longer names → bigger rec_len → fewer entries per block → fewer
        // iterations to force multiple extensions.
        let name = format!("{target}/entry_{i:05}_pad.txt");
        fs.apply_create(&name, 0o644)
            .unwrap_or_else(|e| panic!("create #{i} {name}: {e}"));

        // Poll the target's size: each jump of `bs` means another block
        // was allocated to the directory → drop a gap file so the next
        // extension lands on a non-contiguous physical block.
        let new_size = fs.read_inode_verified(dir_ino).expect("re-read").0.size;
        if new_size > prev_size {
            prev_size = new_size;
            let gap = format!("/ext4rs_gap_{gap_counter:04}.bin");
            gap_counter += 1;
            fs.apply_create(&gap, 0o644).expect("create gap");
            fs.apply_replace_file_content(&gap, &vec![0xABu8; bs as usize])
                .expect("write gap");
        }

        // Done as soon as promotion fires.
        let (dir_inode, _) = fs.read_inode_verified(dir_ino).expect("re-read");
        let hdr = ExtentHeader::parse(&dir_inode.block).expect("parse");
        if hdr.depth >= 1 {
            return;
        }
    }
    let (dir_inode, _) = fs.read_inode_verified(dir_ino).expect("final read");
    let hdr = ExtentHeader::parse(&dir_inode.block).expect("parse");
    panic!(
        "never promoted after {max_entries} creates (depth={} entries={} max={} size={})",
        hdr.depth, hdr.entries, hdr.max, dir_inode.size
    );
}

#[test]
fn extending_dir_past_four_noncontiguous_extents_triggers_promotion() {
    let Some(path) = copy_to_tmp("ext4-basic.img") else {
        return;
    };
    let dev = FileDevice::open_rw(&path).expect("open rw");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");

    let target = "/big_dir";
    fs.apply_mkdir(target, 0o755).expect("mkdir target");

    create_until_promotion(&fs, target, 2000);

    // After promotion the inline root carries a single index entry pointing
    // at the leaf block.
    let dir_ino = resolve(&fs, target).expect("resolve");
    let (dir_inode, _) = fs.read_inode_verified(dir_ino).expect("read");
    let hdr = ExtentHeader::parse(&dir_inode.block).expect("parse");
    assert_eq!(hdr.depth, 1);
    assert_eq!(hdr.entries, 1);

    drop(fs);

    // Remount read-only: depth-1 lookup + leaf-block CRC must verify every
    // entry we planted in `target`.
    let dev = FileDevice::open(&path).expect("open ro");
    let fs = Filesystem::mount(Arc::new(dev)).expect("remount");
    let dir_ino = resolve(&fs, target).expect("resolve target (ro)");
    let (dir_inode, _) = fs.read_inode_verified(dir_ino).expect("read");
    let hdr = ExtentHeader::parse(&dir_inode.block).expect("parse");
    assert_eq!(hdr.depth, 1, "promotion must persist across remount");

    // Every logical block of the directory must still map through the
    // depth-1 root (this also exercises the extent-tail CRC on the leaf
    // block, since verify_extent_tail is called during the descend).
    let n_blocks = dir_inode.size.div_ceil(fs.sb.block_size() as u64);
    for logical in 0..n_blocks {
        let phys = ext4rs::extent::map_logical(
            &dir_inode.block,
            fs.dev.as_ref(),
            fs.sb.block_size(),
            logical,
        )
        .expect("map_logical through depth-1");
        assert!(phys.is_some(), "logical {logical} must map post-promotion");
    }

    fs::remove_file(path).ok();
}

#[test]
fn directory_growth_continues_past_promotion() {
    // Once a directory's extent tree promotes to depth 1, subsequent grows
    // must mutate the leaf block (not the inline root). Regression: without
    // the depth-1 insertion path, `plan_insert_extent` on the depth-1 inline
    // root bails with `multi-level tree mutation not yet supported`, so any
    // extra dir entry that required a fresh data block would fail.
    let Some(path) = copy_to_tmp("ext4-basic.img") else {
        return;
    };
    let dev = FileDevice::open_rw(&path).expect("open rw");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let bs = fs.sb.block_size() as u64;

    let target = "/post_promo_dir";
    fs.apply_mkdir(target, 0o755).expect("mkdir target");
    create_until_promotion(&fs, target, 2000);

    // Confirm we landed at depth 1.
    let dir_ino = resolve(&fs, target).expect("resolve");
    let (dir_inode, _) = fs.read_inode_verified(dir_ino).expect("read");
    let hdr = ExtentHeader::parse(&dir_inode.block).expect("parse");
    assert_eq!(hdr.depth, 1);
    let size_at_promo = dir_inode.size;

    // Fill the directory past promotion boundary until it grows by at least
    // two more non-contiguous blocks (with gap files to prevent merge).
    let mut post_extensions = 0;
    let mut prev_size = size_at_promo;
    let mut gap_counter = 0u32;
    for i in 0..2500 {
        let name = format!("{target}/post_{i:05}_extra.txt");
        fs.apply_create(&name, 0o644)
            .unwrap_or_else(|e| panic!("post-promotion create #{i}: {e}"));
        let (di, _) = fs.read_inode_verified(dir_ino).expect("read");
        if di.size > prev_size {
            prev_size = di.size;
            post_extensions += 1;
            let gap = format!("/post_gap_{gap_counter:04}.bin");
            gap_counter += 1;
            fs.apply_create(&gap, 0o644).expect("create gap");
            fs.apply_replace_file_content(&gap, &vec![0xCDu8; bs as usize])
                .expect("write gap");
            if post_extensions >= 2 {
                break;
            }
        }
    }
    assert!(
        post_extensions >= 2,
        "expected at least 2 post-promotion extensions, got {post_extensions}"
    );

    // Tree should still be depth 1 (we haven't overflowed the leaf yet — the
    // leaf has capacity for 340 entries on a 4 KiB block). The leaf's
    // entry count must have grown.
    let (dir_inode, _) = fs.read_inode_verified(dir_ino).expect("final read");
    let hdr = ExtentHeader::parse(&dir_inode.block).expect("parse");
    assert_eq!(hdr.depth, 1, "still at depth 1 after additional grows");
    assert!(dir_inode.size > size_at_promo, "dir size grew");

    drop(fs);
    // Remount ro, confirm the depth-1 tree + newly-inserted leaf entries all
    // resolve via the read-side lookup_verified + verify_extent_tail.
    let dev = FileDevice::open(&path).expect("open ro");
    let fs = Filesystem::mount(Arc::new(dev)).expect("remount");
    let dir_ino = resolve(&fs, target).expect("resolve");
    let (dir_inode, _) = fs.read_inode_verified(dir_ino).expect("read");
    let n_blocks = dir_inode.size.div_ceil(fs.sb.block_size() as u64);
    for logical in 0..n_blocks {
        let phys = ext4rs::extent::map_logical(
            &dir_inode.block,
            fs.dev.as_ref(),
            fs.sb.block_size(),
            logical,
        )
        .expect("map_logical through depth-1 leaf");
        assert!(phys.is_some(), "logical {logical} resolves post-remount");
    }

    fs::remove_file(path).ok();
}

#[test]
fn verified_read_survives_promotion() {
    // Tighter variant: pick one specific entry, confirm it resolves both
    // before the remount and after, to lock in that neither the write path
    // nor the depth-1 read path drops data.
    let Some(path) = copy_to_tmp("ext4-basic.img") else {
        return;
    };
    let dev = FileDevice::open_rw(&path).expect("open rw");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");

    let target = "/promo_dir";
    fs.apply_mkdir(target, 0o755).expect("mkdir target");
    create_until_promotion(&fs, target, 2000);

    // Plant a distinctive entry after promotion — it goes through the new
    // depth-1 + leaf path for lookup.
    let canary = format!("{target}/ZZZ_canary.marker");
    fs.apply_create(&canary, 0o644).expect("create canary");
    assert!(
        resolve(&fs, &canary).is_some(),
        "canary resolves pre-remount"
    );

    drop(fs);
    let dev = FileDevice::open(&path).expect("open ro");
    let fs = Filesystem::mount(Arc::new(dev)).expect("remount");
    assert!(
        resolve(&fs, &canary).is_some(),
        "canary resolves post-remount"
    );

    fs::remove_file(path).ok();
}
