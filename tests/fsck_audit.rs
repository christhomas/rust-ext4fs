//! Integration tests for the read-only fsck-style audit.

use fs_ext4::block_io::FileDevice;
use fs_ext4::fs::Filesystem;
use fs_ext4::fsck::Anomaly;
use std::sync::Arc;

#[test]
fn pristine_basic_image_audits_clean() {
    let path = "test-disks/ext4-basic.img";
    let file = match FileDevice::open(path) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("skip: {path} not present");
            return;
        }
    };
    let dev: Arc<dyn fs_ext4::block_io::BlockDevice> = Arc::new(file);
    let fs = Filesystem::mount(dev).expect("mount");

    let report = fs.audit(u32::MAX, u32::MAX).expect("audit");
    assert!(
        report.is_clean(),
        "pristine image should audit clean, got {:?}",
        report.anomalies
    );
    assert!(
        report.directories_scanned > 0,
        "must have scanned at least one directory"
    );
    assert!(
        report.inodes_visited >= 2,
        "must have visited root + at least one entry"
    );
}

#[test]
fn htree_image_audits_clean() {
    let path = "test-disks/ext4-htree.img";
    let file = match FileDevice::open(path) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("skip: {path} not present");
            return;
        }
    };
    let dev: Arc<dyn fs_ext4::block_io::BlockDevice> = Arc::new(file);
    let fs = Filesystem::mount(dev).expect("mount");

    // Larger dirs — bound to 10k entries just to keep the test bounded.
    let report = fs.audit(1024, 10_000).expect("audit");
    assert!(
        report.is_clean(),
        "htree image should audit clean, got {:?}",
        report.anomalies
    );
}

#[test]
fn audit_with_zero_bounds_still_succeeds() {
    let path = "test-disks/ext4-basic.img";
    let file = match FileDevice::open(path) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("skip: {path} not present");
            return;
        }
    };
    let dev: Arc<dyn fs_ext4::block_io::BlockDevice> = Arc::new(file);
    let fs = Filesystem::mount(dev).expect("mount");
    let report = fs.audit(0, 0).expect("audit");
    assert_eq!(report.directories_scanned, 0);
    assert_eq!(report.entries_scanned, 0);
    // Zero-bound audits are noisy (nothing scanned, everything looks
    // "too high" from the inode side) but MUST NOT panic.
    let _ = report;
}

#[test]
fn all_pristine_test_images_audit_clean() {
    let images = [
        "test-disks/ext4-basic.img",
        "test-disks/ext4-inline.img",
        "test-disks/ext4-xattr.img",
        "test-disks/ext4-acl.img",
        "test-disks/ext4-deep-extents.img",
        "test-disks/ext4-csum-seed.img",
        "test-disks/ext4-no-csum.img",
    ];
    let mut any_tested = false;
    for path in images {
        let Ok(file) = FileDevice::open(path) else {
            continue;
        };
        any_tested = true;
        let dev: Arc<dyn fs_ext4::block_io::BlockDevice> = Arc::new(file);
        let fs = Filesystem::mount(dev).expect("mount");
        let report = fs.audit(1024, 10_000).expect("audit");
        assert!(
            report.is_clean(),
            "{path} should audit clean, got {:?}",
            report.anomalies
        );
    }
    if !any_tested {
        eprintln!("skip: no pristine images present");
    }
}

#[test]
fn anomaly_variants_are_distinct_debug() {
    // Catch typos in Debug output that could confuse downstream consumers.
    let a = Anomaly::LinkCountTooLow {
        ino: 10,
        stored: 1,
        observed: 2,
    };
    let b = Anomaly::LinkCountTooHigh {
        ino: 10,
        stored: 3,
        observed: 2,
    };
    assert_ne!(format!("{a:?}"), format!("{b:?}"));
}
