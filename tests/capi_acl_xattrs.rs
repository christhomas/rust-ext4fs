//! Regression: the C ABI (listxattr/getxattr) must surface the
//! `system.posix_acl_access` / `system.posix_acl_default` xattrs so Swift
//! FSKit can return raw ACL bytes. This locks in support for name_index 2
//! and 3 in the xattr decoder, which differ from the usual
//! "prefix + suffix" layout.

use fs_ext4::capi::*;
use std::ffi::CString;
use std::os::raw::c_void;
use std::path::Path;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-acl.img");

fn mount_or_skip() -> Option<*mut fs_ext4_fs_t> {
    if !Path::new(IMAGE).exists() {
        eprintln!("skip: {IMAGE} not built");
        return None;
    }
    let p = CString::new(IMAGE).unwrap();
    let fs = unsafe { fs_ext4_mount(p.as_ptr()) };
    if fs.is_null() {
        eprintln!("skip: mount failed on {IMAGE}");
        return None;
    }
    Some(fs)
}

fn names_on(fs: *mut fs_ext4_fs_t, path: &str) -> Vec<String> {
    let p = CString::new(path).unwrap();
    let mut buf = vec![0u8; 1024];
    let n = unsafe {
        fs_ext4_listxattr(
            fs,
            p.as_ptr(),
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };
    if n < 0 {
        let err = unsafe {
            std::ffi::CStr::from_ptr(fs_ext4_last_error())
                .to_string_lossy()
                .into_owned()
        };
        panic!("listxattr failed on {path}: {err}");
    }
    buf.truncate(n as usize);
    buf.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

fn get_size(fs: *mut fs_ext4_fs_t, path: &str, name: &str) -> Option<i64> {
    let p = CString::new(path).unwrap();
    let n = CString::new(name).unwrap();
    let sz = unsafe { fs_ext4_getxattr(fs, p.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) };
    if sz < 0 {
        None
    } else {
        Some(sz)
    }
}

fn get_bytes(fs: *mut fs_ext4_fs_t, path: &str, name: &str) -> Option<Vec<u8>> {
    let sz = get_size(fs, path, name)?;
    let p = CString::new(path).unwrap();
    let n = CString::new(name).unwrap();
    let mut buf = vec![0u8; sz as usize];
    let ret = unsafe {
        fs_ext4_getxattr(
            fs,
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
        )
    };
    assert_eq!(ret, sz);
    Some(buf)
}

#[test]
fn named_txt_exposes_posix_acl_access_via_capi() {
    let Some(fs) = mount_or_skip() else {
        return;
    };
    let names = names_on(fs, "/named.txt");
    assert!(
        names.iter().any(|n| n == "system.posix_acl_access"),
        "expected system.posix_acl_access in listxattr for /named.txt, got {names:?}"
    );
    let bytes =
        get_bytes(fs, "/named.txt", "system.posix_acl_access").expect("getxattr returned -1");
    assert!(!bytes.is_empty(), "acl xattr must be non-empty");
    // Every ext4 ACL blob starts with the 4-byte version = 0x00000002 little-endian.
    assert_eq!(
        &bytes[..4],
        &[0x01, 0x00, 0x00, 0x00],
        "ext4 ACL version header mismatch (EXT4_ACL_VERSION=1)"
    );
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn acl_dir_exposes_posix_acl_default_via_capi() {
    let Some(fs) = mount_or_skip() else {
        return;
    };
    let names = names_on(fs, "/acl_dir");
    assert!(
        names.iter().any(|n| n == "system.posix_acl_default"),
        "expected system.posix_acl_default in listxattr for /acl_dir, got {names:?}"
    );
    let bytes =
        get_bytes(fs, "/acl_dir", "system.posix_acl_default").expect("getxattr returned -1");
    assert_eq!(&bytes[..4], &[0x01, 0x00, 0x00, 0x00]);
    unsafe { fs_ext4_umount(fs) };
}

#[test]
fn plain_txt_has_no_acl_xattrs_via_capi() {
    let Some(fs) = mount_or_skip() else {
        return;
    };
    let names = names_on(fs, "/plain.txt");
    assert!(
        !names.iter().any(|n| n.starts_with("system.posix_acl_")),
        "plain.txt should not carry ACL xattrs: {names:?}"
    );
    unsafe { fs_ext4_umount(fs) };
}
