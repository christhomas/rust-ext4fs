//! Verify the C ABI handles non-UTF-8 and empty path bytes without crashing.
//!
//! cstr_to_str treats non-UTF-8 as "" (via to_str().unwrap_or("")). Empty
//! path resolves to root via split_path drop-empty. Edge case to lock in:
//! mutating ops on empty/invalid paths must NOT corrupt the filesystem —
//! they should refuse cleanly.

use fs_ext4::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

fn scratch() -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(format!(
        "/tmp/fs_ext4_capi_nonutf8_{}_{n}.img",
        std::process::id()
    ));
    let mut out = fs::File::create(&dst).unwrap();
    out.write_all(&fs::read(SRC).unwrap()).unwrap();
    dst
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(fs_ext4_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

fn raw_non_utf8_cstr() -> Vec<c_char> {
    // Bytes: 0xFF 0xFE (invalid UTF-8) + NUL. Must be kept in a Vec so the
    // pointer stays valid for the test's duration.
    // 0xFF, 0xFE, NUL as the platform's `c_char`, which is signed on x86_64
    // and Apple Silicon but unsigned on aarch64-linux — writing them as
    // negative `i8` literals only compiles on the signed half.
    vec![0xFF_u8 as std::ffi::c_char, 0xFE_u8 as std::ffi::c_char, 0]
}

#[test]
fn stat_on_non_utf8_path_does_not_crash() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let bad = raw_non_utf8_cstr();
    let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
    // This should either succeed (non-UTF-8 interpreted as empty → root) or
    // fail, but NEVER panic or segfault. Just confirming we get a clean result.
    let rc = unsafe { fs_ext4_stat(fs_h, bad.as_ptr(), &mut attr) };
    // Either outcome is acceptable; what matters is that we returned and the
    // errno is consistent.
    if rc == 0 {
        // Non-UTF-8 interpreted as empty path → root (inode 2). Tolerable.
        assert_eq!(attr.inode, 2);
    } else {
        assert_ne!(fs_ext4_last_errno(), 0);
    }
    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn create_on_non_utf8_path_does_not_corrupt() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();

    let before_root_inode = {
        let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs_h.is_null());
        let root = CString::new("/").unwrap();
        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_ext4_stat(fs_h, root.as_ptr(), &mut attr) };
        assert_eq!(rc, 0);
        let sz = attr.size;
        unsafe { fs_ext4_umount(fs_h) };
        sz
    };

    {
        let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
        assert!(!fs_h.is_null());
        let bad = raw_non_utf8_cstr();
        let ino = unsafe { fs_ext4_create(fs_h, bad.as_ptr(), 0o644) };
        // Must not crash. Most likely fails (empty path can't be created).
        if ino != 0 {
            // If somehow succeeded, at least verify we didn't break the fs.
            eprintln!(
                "note: create on non-UTF-8 returned ino={ino}, last_err={}",
                last_err()
            );
        } else {
            assert_ne!(fs_ext4_last_errno(), 0);
        }
        unsafe { fs_ext4_umount(fs_h) };
    }

    // Remount ro and confirm root is intact.
    {
        let fs_h = unsafe { fs_ext4_mount(img_c.as_ptr()) };
        assert!(!fs_h.is_null(), "post-test remount: {}", last_err());
        let root = CString::new("/").unwrap();
        let mut attr: fs_ext4_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_ext4_stat(fs_h, root.as_ptr(), &mut attr) };
        assert_eq!(rc, 0);
        assert_eq!(attr.size, before_root_inode, "root size must be unchanged");
        unsafe { fs_ext4_umount(fs_h) };
    }

    let _ = fs::remove_file(&img);
}

#[test]
fn unlink_on_non_utf8_path_fails_cleanly() {
    let img = scratch();
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { fs_ext4_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    let bad = raw_non_utf8_cstr();
    let rc = unsafe { fs_ext4_unlink(fs_h, bad.as_ptr()) };
    assert_eq!(rc, -1, "unlink on non-UTF-8 path must fail");
    assert_ne!(fs_ext4_last_errno(), 0);

    unsafe { fs_ext4_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
