//! Integration tests for `ext4rs_setxattr` (in-inode path).
//!
//! Uses `test-disks/ext4-xattr.img` which has an existing tagged file at
//! `/tagged.txt` (user.color=red, user.com.apple.FinderInfo) and a plain
//! file at `/plain.txt` with no xattrs.

use ext4rs::capi::*;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-xattr.img");

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = PathBuf::from(format!(
        "/tmp/ext4rs_capi_setxattr_{tag}_{}_{n}.img",
        std::process::id()
    ));
    let bytes = fs::read(SRC).expect("read src");
    let mut out = fs::File::create(&dst).expect("create");
    out.write_all(&bytes).expect("write");
    out.flush().expect("flush");
    dst
}

fn last_err() -> String {
    unsafe {
        CStr::from_ptr(ext4rs_last_error())
            .to_string_lossy()
            .into_owned()
    }
}

fn get_xattr(fs: *mut ext4rs_fs_t, path: &str, name: &str) -> Option<Vec<u8>> {
    let p = CString::new(path).unwrap();
    let n = CString::new(name).unwrap();
    let probe = unsafe { ext4rs_getxattr(fs, p.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) };
    if probe < 0 {
        return None;
    }
    if probe == 0 {
        return Some(Vec::new());
    }
    let mut buf = vec![0u8; probe as usize];
    let got = unsafe {
        ext4rs_getxattr(
            fs,
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len(),
        )
    };
    if got < 0 {
        return None;
    }
    Some(buf[..got as usize].to_vec())
}

#[test]
fn setxattr_creates_new_entry_on_plain_file() {
    let img = scratch("create");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.tag").unwrap();
    let value = b"review";

    let fs_h = unsafe { ext4rs_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null(), "mount_rw: {}", last_err());

    let rc = unsafe {
        ext4rs_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, 0, "setxattr: {}", last_err());
    assert_eq!(ext4rs_last_errno(), 0);

    let got = get_xattr(fs_h, "/plain.txt", "user.tag").unwrap();
    assert_eq!(got, value);

    unsafe { ext4rs_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_replaces_existing_entry() {
    let img = scratch("replace");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/tagged.txt").unwrap();
    let name_c = CString::new("user.color").unwrap();

    let fs_h = unsafe { ext4rs_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());

    // Before: user.color=red.
    assert_eq!(
        get_xattr(fs_h, "/tagged.txt", "user.color").unwrap(),
        b"red"
    );

    // Same-length replacement — original value was "red" (3B → padded 4).
    // "sky" is also 3B, so the layout fits regardless of how tight the
    // original region was packed.
    let new = b"sky";
    let rc = unsafe {
        ext4rs_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            new.as_ptr() as *const _,
            new.len(),
        )
    };
    assert_eq!(rc, 0, "setxattr: {}", last_err());

    assert_eq!(
        get_xattr(fs_h, "/tagged.txt", "user.color").unwrap(),
        b"sky"
    );
    // Other xattr (FinderInfo) must survive.
    assert!(get_xattr(fs_h, "/tagged.txt", "user.com.apple.FinderInfo").is_some());

    unsafe { ext4rs_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_persists_across_remount() {
    let img = scratch("remount");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.label").unwrap();
    let value = b"persisted";

    let fs_h = unsafe { ext4rs_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe {
        ext4rs_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, 0);
    unsafe { ext4rs_umount(fs_h) };

    let fs2 = unsafe { ext4rs_mount(img_c.as_ptr()) };
    assert!(!fs2.is_null(), "remount failed — csum not patched?");
    let got = get_xattr(fs2, "/plain.txt", "user.label").unwrap();
    assert_eq!(got, value);
    unsafe { ext4rs_umount(fs2) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_unknown_prefix_returns_einval() {
    let img = scratch("badns");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("strange.name").unwrap();

    let fs_h = unsafe { ext4rs_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let value = b"x";
    let rc = unsafe {
        ext4rs_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(ext4rs_last_errno(), 22);
    unsafe { ext4rs_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_huge_value_returns_enospc() {
    // 512-byte value can't possibly fit in an in-inode region (which
    // maxes out at 128-byte inode - 32 extra_isize = ~96 bytes of room).
    let img = scratch("enospc");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let path_c = CString::new("/plain.txt").unwrap();
    let name_c = CString::new("user.huge").unwrap();
    let value = vec![0xABu8; 512];

    let fs_h = unsafe { ext4rs_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let rc = unsafe {
        ext4rs_setxattr(
            fs_h,
            path_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const _,
            value.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(ext4rs_last_errno(), 28, "ENOSPC expected");
    unsafe { ext4rs_umount(fs_h) };
    let _ = fs::remove_file(&img);
}

#[test]
fn setxattr_null_args_return_einval() {
    let img = scratch("null");
    let img_c = CString::new(img.to_str().unwrap()).unwrap();
    let fs_h = unsafe { ext4rs_mount_rw(img_c.as_ptr()) };
    assert!(!fs_h.is_null());
    let p = CString::new("/plain.txt").unwrap();
    let n = CString::new("user.x").unwrap();
    let rc = unsafe {
        ext4rs_setxattr(
            fs_h,
            std::ptr::null(),
            n.as_ptr(),
            b"v".as_ptr() as *const _,
            1,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(ext4rs_last_errno(), 22);
    let rc = unsafe {
        ext4rs_setxattr(
            fs_h,
            p.as_ptr(),
            std::ptr::null(),
            b"v".as_ptr() as *const _,
            1,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(ext4rs_last_errno(), 22);
    unsafe { ext4rs_umount(fs_h) };
    let _ = fs::remove_file(&img);
}
