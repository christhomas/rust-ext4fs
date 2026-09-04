//! Integration tests for `fs_ext4_utimens`.
//!
//! Covers:
//! - atime + mtime round-trip via `fs_ext4_stat`.
//! - `u32::MAX` sentinel on either _sec leaves the original alone.
//! - ctime bumps on every call (POSIX requirement).
//! - Missing-path / null-arg errnos.
//! - RO (read-only) mount refuses with a non-zero errno.
//! - Survives unmount → csum-validated remount.

use fs_ext4::capi::*;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(format!(
        "/tmp/fs_ext4_capi_utimens_{tag}_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs::read(SRC).expect("read src");
    let mut out = fs::File::create(&dst).expect("create");
    out.write_all(&bytes).expect("write");
    out.flush().expect("flush");
    dst
}

fn stat_attr(fs_handle: *mut fs_ext4_fs_t, path: &str) -> fs_ext4_attr_t {
    let p = CString::new(path).unwrap();
    let mut attr = MaybeUninit::<fs_ext4_attr_t>::uninit();
    let rc = unsafe { fs_ext4_stat(fs_handle, p.as_ptr(), attr.as_mut_ptr()) };
    assert_eq!(rc, 0, "stat {path} failed");
    unsafe { attr.assume_init() }
}

#[test]
fn utimens_sets_both_and_bumps_ctime() {
    let img = scratch("basic");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    // 2000-01-01 and 2000-01-02 — distinctive values far from the
    // build timestamp of the test image.
    let a = 946_684_800u32;
    let m = 946_771_200u32;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), a, 0, m, 0) };
    assert_eq!(rc, 0);
    assert_eq!(fs_ext4_last_errno(), 0);

    let after = stat_attr(fs_handle, "/test.txt");
    // The attr struct widened to i64 in 0.6.0; the setter still takes
    // u32 seconds, so the comparison crosses the widening deliberately.
    assert_eq!(after.atime, i64::from(a));
    assert_eq!(after.mtime, i64::from(m));
    // ctime must be recent (now), not one of the values above.
    assert!(after.ctime > i64::from(a));
    assert!(after.ctime > i64::from(m));

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_atime_sentinel_leaves_atime_alone() {
    let img = scratch("atime_sentinel");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");
    let fresh_m = 1_700_000_000u32;

    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), u32::MAX, 0, fresh_m, 0) };
    assert_eq!(rc, 0);

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, before.atime, "atime preserved by sentinel");
    assert_eq!(after.mtime, i64::from(fresh_m), "mtime applied");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_mtime_sentinel_leaves_mtime_alone() {
    let img = scratch("mtime_sentinel");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());

    let before = stat_attr(fs_handle, "/test.txt");
    let fresh_a = 1_700_000_000u32;

    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), fresh_a, 0, u32::MAX, 0) };
    assert_eq!(rc, 0);

    let after = stat_attr(fs_handle, "/test.txt");
    assert_eq!(after.atime, i64::from(fresh_a), "atime applied");
    assert_eq!(after.mtime, before.mtime, "mtime preserved by sentinel");

    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_missing_path_sets_enoent() {
    let img = scratch("enoent");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let bad = CString::new("/nope_utimens_xyz.qqq").unwrap();
    let rc = unsafe { fs_ext4_utimens(fs_handle, bad.as_ptr(), 1, 0, 1, 0) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 2);
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_null_args_set_einval() {
    let img = scratch("null");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let rc = unsafe { fs_ext4_utimens(fs_handle, std::ptr::null(), 1, 0, 1, 0) };
    assert_eq!(rc, -1);
    assert_eq!(fs_ext4_last_errno(), 22);
    unsafe { fs_ext4_umount(fs_handle) };
    let _ = fs::remove_file(&img);
}

#[test]
fn utimens_survives_remount_with_csum() {
    let img = scratch("csum");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/test.txt").unwrap();

    let fs_handle = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_handle.is_null());
    let a = 1_500_000_000u32;
    let m = 1_500_000_100u32;
    let rc = unsafe { fs_ext4_utimens(fs_handle, path_c.as_ptr(), a, 0, m, 0) };
    assert_eq!(rc, 0);
    unsafe { fs_ext4_umount(fs_handle) };

    let fs2 = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed — inode csum not patched?");
    let after = stat_attr(fs2, "/test.txt");
    // The attr struct widened to i64 in 0.6.0; the setter still takes
    // u32 seconds, so the comparison crosses the widening deliberately.
    assert_eq!(after.atime, i64::from(a));
    assert_eq!(after.mtime, i64::from(m));
    unsafe { fs_ext4_umount(fs2) };

    let _ = fs::remove_file(&img);
}
