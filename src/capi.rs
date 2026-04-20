//! C ABI exports — MUST match `include/fs_ext4.h` exactly. Consumers
//! link `libfs_ext4.a` and #include that header; any signature change
//! here requires the header to change in lockstep.
//!
//! Phase 1 (read-only) surface:
//! - fs_ext4_mount(device_path) -> *mut fs_ext4_fs_t
//! - fs_ext4_mount_with_callbacks(cfg) -> *mut fs_ext4_fs_t
//! - fs_ext4_umount(fs)
//! - fs_ext4_get_volume_info(fs, info) -> int
//! - fs_ext4_stat(fs, path, attr) -> int
//! - fs_ext4_dir_open(fs, path) -> *mut iter
//! - fs_ext4_dir_next(iter) -> *const dirent
//! - fs_ext4_dir_close(iter)
//! - fs_ext4_read_file(fs, ...) -> i64 (extents + inline_data)
//! - fs_ext4_readlink(fs, path, buf, bufsize) -> int
//! - fs_ext4_listxattr(fs, path, buf, bufsize) -> i64
//! - fs_ext4_getxattr(fs, path, name, buf, bufsize) -> i64
//! - fs_ext4_last_error() -> *const c_char
//! - fs_ext4_last_errno() -> c_int          (POSIX errno companion to last_error)
//!
//! Phase 4 (write path, in progress):
//! - fs_ext4_mount_rw(device_path) -> *mut fs_ext4_fs_t
//! - fs_ext4_truncate(fs, path, new_size) -> int (shrink + sparse grow)
//! - fs_ext4_symlink(fs, target, linkpath) -> u32 inode (fast + slow path)
//! - fs_ext4_chmod(fs, path, mode) -> int
//! - fs_ext4_chown(fs, path, uid, gid) -> int
//! - fs_ext4_utimens(fs, path, atime_sec, atime_nsec, mtime_sec, mtime_nsec) -> int
//! - fs_ext4_unlink(fs, path) -> int
//! - fs_ext4_write_file(fs, path, data, len) -> i64 (save-as replace body)
//!
//! Memory ownership rules (from ntfsbridge precedent, documented in docs/ext4-rs-capi.md):
//! - `fs_ext4_fs_t*` is owned by the caller. Freed via `fs_ext4_umount`
//!   (use for both `mount`, `mount_with_callbacks`, and `mount_rw` handles).
//! - `fs_ext4_dir_iter_t*` is owned by the caller. Freed via `fs_ext4_dir_close`.
//! - `fs_ext4_dir_next` returns a pointer into the iterator's internal buffer;
//!   valid until the next `fs_ext4_dir_next` or `fs_ext4_dir_close` call.
//! - `fs_ext4_last_error` / `fs_ext4_last_errno` read thread-local
//!   storage; valid until the next FFI call on the same thread.

#![allow(non_camel_case_types)]
// Module-level docs (above) cover the FFI memory-ownership contract for
// every exported unsafe fn; per-function `# Safety` sections would be
// near-duplicates.
#![allow(clippy::missing_safety_doc)]

use crate::block_io::{BlockDevice, CallbackDevice, FileDevice};
use crate::dir::{self, DirBlockIter, DirEntryType};
use crate::error::errno::{EINVAL, EISDIR, ENOENT, ENOTDIR};
use crate::error::{Error, Result};
use crate::extent;
use crate::features;
use crate::file_io;
use crate::fs::Filesystem;
use crate::inode::{Inode, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK};
use crate::path as path_mod;
use crate::xattr;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

// ===========================================================================
// Thread-local last error
// ===========================================================================

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
    static LAST_ERRNO: RefCell<c_int> = const { RefCell::new(0) };
}

fn set_last_error<E: std::fmt::Display>(e: E) {
    let msg = format!("{e}");
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() =
            CString::new(msg).unwrap_or_else(|_| CString::new("unknown error").unwrap());
    });
}

fn set_last_errno(errno: c_int) {
    LAST_ERRNO.with(|cell| *cell.borrow_mut() = errno);
}

/// Record both the error string (with context) and the POSIX errno.
/// Call this instead of `set_last_error` whenever the source is an `Error`.
fn set_err_from(err: &Error, context: &str) {
    set_last_error(format!("{context}: {err}"));
    set_last_errno(err.to_errno());
}

/// Record a string message and an explicit errno. Use for validation failures
/// (null args, wrong file type) where there is no underlying `Error`.
fn set_err_msg(msg: &str, errno: c_int) {
    set_last_error(msg);
    set_last_errno(errno);
}

fn clear_last_error() {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = CString::new("").unwrap();
    });
    LAST_ERRNO.with(|cell| *cell.borrow_mut() = 0);
}

/// Wrap an FFI body in `catch_unwind`. If the body panics, record the panic
/// message in `last_error` and return `fail`. This prevents unwinding across
/// the C ABI boundary (undefined behaviour).
fn ffi_guard<T>(fail: T, body: impl FnOnce() -> T + std::panic::UnwindSafe) -> T {
    match std::panic::catch_unwind(body) {
        Ok(v) => v,
        Err(panic) => {
            let msg = if let Some(s) = panic.downcast_ref::<&'static str>() {
                format!("panic: {s}")
            } else if let Some(s) = panic.downcast_ref::<String>() {
                format!("panic: {s}")
            } else {
                "panic: (non-string payload)".to_string()
            };
            set_err_msg(&msg, crate::error::errno::EIO);
            fail
        }
    }
}

/// Get the last error message for the current thread.
/// Returns a pointer valid until the next FFI call on this thread.
#[no_mangle]
pub extern "C" fn fs_ext4_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| cell.borrow().as_ptr())
}

/// Get the POSIX errno for the last failed FFI call on this thread.
/// Returns 0 if the last call succeeded (or no call has been made yet).
/// Codes: ENOENT (2), EIO (5), ENOTDIR (20), EINVAL (22), ENOTSUP (45),
/// or any errno surfaced by the underlying I/O layer.
#[no_mangle]
pub extern "C" fn fs_ext4_last_errno() -> c_int {
    LAST_ERRNO.with(|cell| *cell.borrow())
}

// ===========================================================================
// ABI types — MUST match include/fs_ext4.h
// ===========================================================================

/// File type (matches `fs_ext4_file_type_t` in the header).
#[repr(C)]
#[derive(Copy, Clone)]
pub enum fs_ext4_file_type_t {
    Unknown = 0,
    RegFile = 1,
    Dir = 2,
    ChrDev = 3,
    BlkDev = 4,
    Fifo = 5,
    Sock = 6,
    Symlink = 7,
}

/// File/directory attributes (matches `fs_ext4_attr_t`).
#[repr(C)]
pub struct fs_ext4_attr_t {
    pub inode: u32,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub crtime: u32,
    pub link_count: u16,
    pub file_type: fs_ext4_file_type_t,
}

/// Directory entry (matches `fs_ext4_dirent_t`).
#[repr(C)]
pub struct fs_ext4_dirent_t {
    pub inode: u32,
    pub file_type: u8,
    pub name_len: u8,
    pub name: [c_char; 256],
}

/// Volume info (matches `fs_ext4_volume_info_t`).
#[repr(C)]
pub struct fs_ext4_volume_info_t {
    pub volume_name: [c_char; 16],
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u32,
    pub free_inodes: u32,
    /// `1` if the filesystem was NOT cleanly unmounted last time it was
    /// used (dirty) — the caller should surface this to the user and
    /// run fsck / journal replay before permitting writes. `0` if the
    /// filesystem is clean. Captured from the on-disk `s_state` field
    /// at mount time, before any journal replay the driver may perform.
    pub mounted_dirty: u8,
}

/// Block device read callback (matches `fs_ext4_read_fn`).
pub type fs_ext4_read_fn = Option<
    unsafe extern "C" fn(context: *mut c_void, buf: *mut c_void, offset: u64, length: u64) -> c_int,
>;

/// Callback-based mount config (matches `fs_ext4_blockdev_cfg_t`).
#[repr(C)]
pub struct fs_ext4_blockdev_cfg_t {
    pub read: fs_ext4_read_fn,
    pub context: *mut c_void,
    pub size_bytes: u64,
    pub block_size: u32,
}

// ===========================================================================
// Opaque handle types
// ===========================================================================

/// Opaque mounted filesystem handle. The caller treats this as `fs_ext4_fs_t*`.
pub struct fs_ext4_fs_t {
    fs: Filesystem,
}

/// Opaque directory iterator handle. The caller treats this as `fs_ext4_dir_iter_t*`.
pub struct fs_ext4_dir_iter_t {
    /// Pre-collected entries (Phase 1 simplicity — streaming can come later).
    entries: Vec<fs_ext4_dirent_t>,
    /// Current position in `entries`.
    position: usize,
    /// Last returned entry — backing storage for the pointer returned from `_dir_next`.
    current: fs_ext4_dirent_t,
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Convert a `*const c_char` to a Rust string. Returns empty string on null.
unsafe fn cstr_to_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    CStr::from_ptr(p).to_str().unwrap_or("")
}

/// Convert POSIX mode bits to `fs_ext4_file_type_t`.
fn mode_to_file_type(mode: u16) -> fs_ext4_file_type_t {
    match mode & S_IFMT {
        S_IFREG => fs_ext4_file_type_t::RegFile,
        S_IFDIR => fs_ext4_file_type_t::Dir,
        S_IFLNK => fs_ext4_file_type_t::Symlink,
        S_IFCHR => fs_ext4_file_type_t::ChrDev,
        S_IFBLK => fs_ext4_file_type_t::BlkDev,
        S_IFIFO => fs_ext4_file_type_t::Fifo,
        S_IFSOCK => fs_ext4_file_type_t::Sock,
        _ => fs_ext4_file_type_t::Unknown,
    }
}

/// Fill an `fs_ext4_attr_t` from an inode.
fn fill_attr(out: &mut fs_ext4_attr_t, ino: u32, inode: &Inode) {
    out.inode = ino;
    out.mode = inode.mode & 0x0FFF; // keep permission bits
    out.uid = inode.uid;
    out.gid = inode.gid;
    out.size = inode.size;
    out.atime = inode.atime;
    out.mtime = inode.mtime;
    out.ctime = inode.ctime;
    out.crtime = inode.crtime;
    out.link_count = inode.links_count;
    out.file_type = mode_to_file_type(inode.mode);
}

// ===========================================================================
// Lifecycle
// ===========================================================================

/// Mount an ext4 filesystem from a device path. Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_mount(device_path: *const c_char) -> *mut fs_ext4_fs_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            clear_last_error();
            let path = cstr_to_str(device_path);
            if path.is_empty() {
                set_err_msg("null or empty device_path", EINVAL);
                return std::ptr::null_mut();
            }

            let dev = match FileDevice::open(path) {
                Ok(d) => Arc::new(d) as Arc<dyn BlockDevice>,
                Err(e) => {
                    set_err_from(&e, &format!("open {path}"));
                    return std::ptr::null_mut();
                }
            };

            match Filesystem::mount(dev) {
                Ok(fs) => Box::into_raw(Box::new(fs_ext4_fs_t { fs })),
                Err(e) => {
                    set_err_from(&e, &format!("mount {path}"));
                    std::ptr::null_mut()
                }
            }
        }),
    )
}

/// Mount via a caller-supplied read callback.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_mount_with_callbacks(
    cfg: *const fs_ext4_blockdev_cfg_t,
) -> *mut fs_ext4_fs_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| mount_with_callbacks_inner(cfg)),
    )
}

unsafe fn mount_with_callbacks_inner(cfg: *const fs_ext4_blockdev_cfg_t) -> *mut fs_ext4_fs_t {
    clear_last_error();
    if cfg.is_null() {
        set_err_msg("null cfg", EINVAL);
        return std::ptr::null_mut();
    }
    let cfg = &*cfg;
    let Some(read_fn) = cfg.read else {
        set_err_msg("cfg.read is null", EINVAL);
        return std::ptr::null_mut();
    };

    // Wrap the C context + callback in a thread-safe closure.
    // The caller is responsible for context lifetime ≥ fs lifetime.
    // We store context as usize to make the closure Send+Sync; Swift/C side
    // is expected to keep the context pointer valid (FSKit guarantees serial
    // access from the extension's queue).
    let ctx_addr = cfg.context as usize;
    let size = cfg.size_bytes;

    let dev = CallbackDevice {
        size,
        read: Box::new(move |offset, buf| {
            let rc = unsafe {
                read_fn(
                    ctx_addr as *mut c_void,
                    buf.as_mut_ptr() as *mut c_void,
                    offset,
                    buf.len() as u64,
                )
            };
            if rc != 0 {
                Err(std::io::Error::other(format!("callback returned {rc}")))
            } else {
                Ok(())
            }
        }),
        // Swift-side callback mount is read-only for now (no write callback
        // plumbed through the C struct). Phase 4 writes go through a
        // different C entry point that accepts a write_fn.
        write: None,
        flush: None,
    };

    match Filesystem::mount(Arc::new(dev) as Arc<dyn BlockDevice>) {
        Ok(fs) => Box::into_raw(Box::new(fs_ext4_fs_t { fs })),
        Err(e) => {
            set_err_from(&e, "mount (callback)");
            std::ptr::null_mut()
        }
    }
}

/// Unmount and free the filesystem handle.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_umount(fs: *mut fs_ext4_fs_t) {
    ffi_guard(
        (),
        AssertUnwindSafe(|| {
            if !fs.is_null() {
                drop(Box::from_raw(fs));
            }
        }),
    )
}

// ===========================================================================
// Volume info
// ===========================================================================

/// Fill `info` with volume statistics. Returns 0 on success, -1 on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_get_volume_info(
    fs: *mut fs_ext4_fs_t,
    info: *mut fs_ext4_volume_info_t,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || info.is_null() {
                set_err_msg("null fs or info", EINVAL);
                return -1;
            }
            let fs = &(*fs).fs;
            let info = &mut *info;

            // Zero the struct
            std::ptr::write_bytes(info as *mut fs_ext4_volume_info_t, 0, 1);

            // Copy volume name (up to 16 bytes incl. NUL)
            let name_bytes = fs.sb.volume_name.as_bytes();
            let copy_len = name_bytes.len().min(15);
            for (i, &b) in name_bytes[..copy_len].iter().enumerate() {
                info.volume_name[i] = b as c_char;
            }
            info.volume_name[copy_len] = 0;

            info.block_size = fs.sb.block_size();
            info.total_blocks = fs.sb.blocks_count;
            info.free_blocks = fs.sb.free_blocks_count;
            info.total_inodes = fs.sb.inodes_count;
            info.free_inodes = fs.sb.free_inodes_count;
            info.mounted_dirty = if fs.sb.is_clean() { 0 } else { 1 };

            0
        }),
    )
}

// ===========================================================================
// Stat / readdir / read — STUBBED until dir.rs + extent.rs land
// ===========================================================================

/// Resolve a path to an inode number via `path::lookup`.
///
/// Each intermediate inode read goes through `Filesystem::read_inode_verified`
/// so the path-walk surfaces `Error::BadChecksum` if any directory inode is
/// corrupt (when `RO_COMPAT_METADATA_CSUM` is enabled).
fn resolve_path(fs: &Filesystem, path: &str) -> Result<u32> {
    let mut reader = |ino: u32| fs.read_inode_verified(ino).map(|(inode, _)| inode);
    let ino = path_mod::lookup_with_csum(fs.dev.as_ref(), &fs.sb, &mut reader, path, &fs.csum)?;

    // POSIX: a trailing slash implies the caller expects a directory. If the
    // resolved target is not a directory, surface ENOTDIR. `path::lookup`
    // drops trailing empty components so this has to be re-checked here.
    // Root (`/`) short-circuits trivially since inode 2 is always a dir.
    if path.ends_with('/') && path != "/" {
        let (inode, _raw) = fs.read_inode_verified(ino)?;
        if !inode.is_dir() {
            return Err(Error::NotADirectory);
        }
    }
    Ok(ino)
}

/// Stat a path. Returns 0 on success, -1 on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_stat(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    attr: *mut fs_ext4_attr_t,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || attr.is_null() {
                set_err_msg("null fs, path, or attr", EINVAL);
                return -1;
            }
            let fs = &(*fs).fs;
            let path = cstr_to_str(path);
            let attr = &mut *attr;

            let ino = match resolve_path(fs, path) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("stat {path}"));
                    return -1;
                }
            };

            let (inode, _raw) = match fs.read_inode_verified(ino) {
                Ok(p) => p,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return -1;
                }
            };

            fill_attr(attr, ino, &inode);
            0
        }),
    )
}

/// Open a directory for iteration. Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_dir_open(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
) -> *mut fs_ext4_dir_iter_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs or path", EINVAL);
                return std::ptr::null_mut();
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);

            let ino = match resolve_path(fs_ref, path_str) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("dir_open {path_str}"));
                    return std::ptr::null_mut();
                }
            };
            let (inode, _raw) = match fs_ref.read_inode_verified(ino) {
                Ok(p) => p,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return std::ptr::null_mut();
                }
            };
            if !inode.is_dir() {
                set_err_msg(&format!("dir_open {path_str}: not a directory"), ENOTDIR);
                return std::ptr::null_mut();
            }

            // Collect entries from all dir data blocks.
            let entries = match collect_dir_entries(fs_ref, &inode) {
                Ok(e) => e,
                Err(e) => {
                    set_err_from(&e, &format!("read directory {path_str}"));
                    return std::ptr::null_mut();
                }
            };

            let iter = Box::new(fs_ext4_dir_iter_t {
                entries,
                position: 0,
                current: std::mem::zeroed(),
            });
            Box::into_raw(iter)
        }),
    )
}

/// Read all directory entries from an inode into `fs_ext4_dirent_t`s.
fn collect_dir_entries(fs: &Filesystem, inode: &Inode) -> Result<Vec<fs_ext4_dirent_t>> {
    if !inode.has_extents() {
        return Err(Error::Corrupt("legacy (non-extent) dirs not yet supported"));
    }
    let block_size = fs.sb.block_size();
    let has_filetype = fs.sb.feature_incompat & features::Incompat::FILETYPE.bits() != 0;

    let mut entries = Vec::new();

    // Handle inline-data dirs (tiny dirs stored inside the inode itself)
    if inode.has_inline_data() {
        for entry in DirBlockIter::new(&inode.block, has_filetype) {
            let e = entry?;
            entries.push(dir_entry_to_bridge(&e));
        }
        return Ok(entries);
    }

    let total_blocks = inode.size.div_ceil(block_size as u64);
    let mut block_buf = vec![0u8; block_size as usize];

    for logical in 0..total_blocks {
        let phys = match extent::map_logical(&inode.block, fs.dev.as_ref(), block_size, logical)? {
            Some(p) => p,
            None => continue, // sparse hole
        };
        fs.dev.read_at(phys * block_size as u64, &mut block_buf)?;

        for entry in DirBlockIter::new(&block_buf, has_filetype) {
            let e = entry?;
            entries.push(dir_entry_to_bridge(&e));
        }
    }

    Ok(entries)
}

/// Convert a parsed DirEntry to the C ABI dirent struct.
fn dir_entry_to_bridge(e: &dir::DirEntry) -> fs_ext4_dirent_t {
    let mut name = [0i8; 256];
    let copy_len = e.name.len().min(255);
    for (i, &b) in e.name[..copy_len].iter().enumerate() {
        name[i] = b as c_char;
    }
    name[copy_len] = 0;

    let file_type = match e.file_type {
        DirEntryType::RegFile => 1u8,
        DirEntryType::Directory => 2,
        DirEntryType::CharDev => 3,
        DirEntryType::BlockDev => 4,
        DirEntryType::Fifo => 5,
        DirEntryType::Socket => 6,
        DirEntryType::Symlink => 7,
        DirEntryType::Unknown => 0,
    };

    fs_ext4_dirent_t {
        inode: e.inode,
        file_type,
        name_len: copy_len as u8,
        name,
    }
}

/// Get the next dir entry. Returns NULL at end or on error.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_dir_next(
    iter: *mut fs_ext4_dir_iter_t,
) -> *const fs_ext4_dirent_t {
    ffi_guard(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            if iter.is_null() {
                return std::ptr::null();
            }
            let iter = &mut *iter;
            if iter.position >= iter.entries.len() {
                return std::ptr::null();
            }
            // Copy into the iterator's `current` buffer so the returned pointer
            // remains valid until the next _dir_next / _dir_close call.
            iter.current = fs_ext4_dirent_t {
                inode: iter.entries[iter.position].inode,
                file_type: iter.entries[iter.position].file_type,
                name_len: iter.entries[iter.position].name_len,
                name: iter.entries[iter.position].name,
            };
            iter.position += 1;
            &iter.current
        }),
    )
}

/// Close a directory iterator.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_dir_close(iter: *mut fs_ext4_dir_iter_t) {
    ffi_guard(
        (),
        AssertUnwindSafe(|| {
            if !iter.is_null() {
                drop(Box::from_raw(iter));
            }
        }),
    )
}

/// Read bytes from a file. Returns bytes read, or -1 on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_read_file(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> i64 {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || buf.is_null() {
                set_err_msg("null fs, path, or buf", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);

            let ino = match resolve_path(fs_ref, path_str) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("read_file {path_str}"));
                    return -1;
                }
            };
            let (inode, inode_raw) = match fs_ref.read_inode_verified(ino) {
                Ok(p) => p,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return -1;
                }
            };
            if !inode.is_file() {
                set_err_msg(&format!("read_file {path_str}: not a regular file"), EINVAL);
                return -1;
            }

            let out = std::slice::from_raw_parts_mut(buf as *mut u8, length as usize);
            match file_io::read_with_raw_verified(
                fs_ref, &inode, &inode_raw, ino, offset, length, out,
            ) {
                Ok(n) => n as i64,
                Err(e) => {
                    set_err_from(&e, &format!("read_file {path_str}"));
                    -1
                }
            }
        }),
    )
}

/// Read a symlink target. Returns 0 on success, -1 on failure.
/// Handles both fast symlinks (target stored inline in i_block, size < 60 bytes)
/// and long symlinks (target stored in data blocks, read via file_io).
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_readlink(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    buf: *mut c_char,
    bufsize: usize,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || buf.is_null() || bufsize == 0 {
                set_err_msg("null fs/path/buf or zero bufsize", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);

            let ino = match resolve_path(fs_ref, path_str) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("readlink {path_str}"));
                    return -1;
                }
            };
            let (inode, _raw) = match fs_ref.read_inode_verified(ino) {
                Ok(p) => p,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return -1;
                }
            };
            if !inode.is_symlink() {
                set_err_msg(&format!("readlink {path_str}: not a symlink"), EINVAL);
                return -1;
            }

            // Fast symlink: target < 60 bytes, stored inline in i_block.
            // Long symlink: target stored in data blocks, read via file_io.
            let target = if inode.size < 60 {
                inode.block[..inode.size as usize].to_vec()
            } else {
                let mut out = vec![0u8; inode.size as usize];
                match file_io::read_verified(fs_ref, &inode, ino, 0, inode.size, &mut out) {
                    Ok(_) => out,
                    Err(e) => {
                        set_err_from(&e, &format!("readlink {path_str}"));
                        return -1;
                    }
                }
            };

            // Copy to output buffer with null terminator, truncating if needed.
            let copy_len = target.len().min(bufsize - 1);
            let out = std::slice::from_raw_parts_mut(buf as *mut u8, bufsize);
            out[..copy_len].copy_from_slice(&target[..copy_len]);
            out[copy_len] = 0;

            0
        }),
    )
}

// ===========================================================================
// Extended attributes
// ===========================================================================

/// List xattr names for a path. NUL-separated, fully-qualified.
/// Returns required total bytes (so callers can probe with NULL/0).
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_listxattr(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    buf: *mut c_char,
    bufsize: usize,
) -> i64 {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs or path", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);

            let ino = match resolve_path(fs_ref, path_str) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("listxattr {path_str}"));
                    return -1;
                }
            };
            let (inode, inode_raw) = match fs_ref.read_inode_verified(ino) {
                Ok(p) => p,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return -1;
                }
            };

            let entries = match xattr::read_all(
                fs_ref.dev.as_ref(),
                &inode,
                &inode_raw,
                fs_ref.sb.inode_size,
                fs_ref.sb.block_size(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    set_err_from(&e, &format!("listxattr {path_str}"));
                    return -1;
                }
            };

            let required: usize = entries.iter().map(|e| e.name.len() + 1).sum();

            if !buf.is_null() && bufsize > 0 {
                let out = std::slice::from_raw_parts_mut(buf as *mut u8, bufsize);
                let mut pos = 0;
                for e in &entries {
                    let name_bytes = e.name.as_bytes();
                    let needed = name_bytes.len() + 1;
                    if pos + needed > bufsize {
                        break;
                    }
                    out[pos..pos + name_bytes.len()].copy_from_slice(name_bytes);
                    out[pos + name_bytes.len()] = 0;
                    pos += needed;
                }
            }

            required as i64
        }),
    )
}

/// Get a single xattr value by fully-qualified name.
/// Returns value size (so callers can probe with NULL/0), or -1 if missing / error.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_getxattr(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    name: *const c_char,
    buf: *mut c_void,
    bufsize: usize,
) -> i64 {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || name.is_null() {
                set_err_msg("null fs, path, or name", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            let name_str = cstr_to_str(name);

            let ino = match resolve_path(fs_ref, path_str) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("getxattr {path_str}"));
                    return -1;
                }
            };
            let (inode, inode_raw) = match fs_ref.read_inode_verified(ino) {
                Ok(p) => p,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return -1;
                }
            };

            let value = match xattr::get(
                fs_ref.dev.as_ref(),
                &inode,
                &inode_raw,
                fs_ref.sb.inode_size,
                fs_ref.sb.block_size(),
                name_str,
            ) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    set_err_msg(
                        &format!("getxattr {path_str}: {name_str} not found"),
                        ENOENT,
                    );
                    return -1;
                }
                Err(e) => {
                    set_err_from(&e, &format!("getxattr {path_str} {name_str}"));
                    return -1;
                }
            };

            if !buf.is_null() && bufsize > 0 {
                let copy_len = value.len().min(bufsize);
                let out = std::slice::from_raw_parts_mut(buf as *mut u8, bufsize);
                out[..copy_len].copy_from_slice(&value[..copy_len]);
            }

            value.len() as i64
        }),
    )
}

// ===========================================================================
// Write path — Phase 4 surface. Currently only truncate-shrink is wired;
// create/unlink/write_file follow as the write path matures.
// ===========================================================================

/// Truncate a file to `new_size`. Only valid when the device was mounted
/// R/W (e.g. via `fs_ext4_mount_rw`). Returns 0 on success, -1 on failure
/// with details in `fs_ext4_last_error`.
///
/// Both directions are supported:
/// - **Shrink** (`new_size < inode.size`): frees the dropped extents,
///   updates block-bitmap + BGD + SB counters, patches i_size +
///   i_blocks + inode csum.
/// - **Sparse grow** (`new_size >= inode.size`): pure metadata update;
///   ext4's extent tree treats unmapped logical blocks as zero-filled
///   holes, so no block allocation happens. Only i_size, i_mtime,
///   i_ctime, and the inode checksum change.
///
/// Refuses directories (POSIX EISDIR); refuses symlinks and special
/// files (EINVAL).
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_truncate(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    new_size: u64,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            let ino = match resolve_path(fs_ref, path_str) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("truncate {path_str}"));
                    return -1;
                }
            };
            // Type guard: truncating a directory corrupts it (frees data blocks,
            // loses . and .. entries). POSIX ftruncate(2) mandates EISDIR on dir.
            // Symlinks, devices, sockets are also not truncatable → EINVAL.
            let inode = match fs_ref.read_inode_verified(ino) {
                Ok((i, _)) => i,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return -1;
                }
            };
            if inode.is_dir() {
                set_err_msg(&format!("truncate {path_str}: is a directory"), EISDIR);
                return -1;
            }
            if !inode.is_file() {
                set_err_msg(&format!("truncate {path_str}: not a regular file"), EINVAL);
                return -1;
            }
            // Dispatch to grow (sparse) or shrink based on direction. At
            // equality either path works; grow wins since it only bumps
            // timestamps.
            let res = if new_size >= inode.size {
                fs_ref.apply_truncate_grow(ino, new_size)
            } else {
                fs_ref.apply_truncate_shrink(ino, new_size)
            };
            match res {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("truncate {path_str} -> {new_size}"));
                    -1
                }
            }
        }),
    )
}

/// Remove a file entry at `path`. Requires a R/W mount.
///
/// Refuses directories (caller should use a future `fs_ext4_rmdir`).
/// Decrements `i_links_count`; if the count reaches zero the inode's data
/// blocks are freed, its bitmap slot is cleared, and the inode body is
/// zeroed with `i_dtime = now` (matching the kernel's unlink convention).
///
/// Returns 0 on success, -1 on failure with details in
/// `fs_ext4_last_error`.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_unlink(fs: *mut fs_ext4_fs_t, path: *const c_char) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            match fs_ref.apply_unlink(path_str) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("unlink {path_str}"));
                    -1
                }
            }
        }),
    )
}

/// Create a new empty regular file at `path` with permission bits `mode`
/// (e.g. 0o644). Parent must exist and be a directory; path must not already
/// exist. Returns the allocated inode number on success (> 0), or 0 on failure
/// with details in `fs_ext4_last_error`.
///
/// We return `u32` rather than `c_int` so Swift sees a plain uint32_t —
/// matches the convention for other inode-returning exports.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_create(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    mode: u16,
) -> u32 {
    ffi_guard(
        0u32,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return 0u32;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            match fs_ref.apply_create(path_str, mode) {
                Ok(ino) => ino,
                Err(e) => {
                    set_err_from(&e, &format!("create {path_str}"));
                    0u32
                }
            }
        }),
    )
}

/// Replace the content of `path` with `len` bytes from `data`. The file
/// must already exist. Frees every existing extent, allocates one
/// contiguous run for the new bytes, writes the payload (zero-padded in
/// the last block), and updates size/mtime. Returns the new size on
/// success or -1 on failure.
///
/// This is the "save-as" path: atomic replacement of a file's body.
/// Appends, sparse writes, and partial overwrites are follow-up work.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_write_file(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    data: *const c_void,
    len: u64,
) -> i64 {
    ffi_guard(
        -1i64,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return -1;
            }
            if data.is_null() && len > 0 {
                set_err_msg("null data with non-zero len", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            // Type guard at the capi level — mirrors fs_ext4_truncate so the
            // caller gets EISDIR/EINVAL instead of Error::Corrupt → EIO when
            // the target is the wrong kind of file.
            let ino = match resolve_path(fs_ref, path_str) {
                Ok(n) => n,
                Err(e) => {
                    set_err_from(&e, &format!("write_file {path_str}"));
                    return -1;
                }
            };
            let inode = match fs_ref.read_inode_verified(ino) {
                Ok((i, _)) => i,
                Err(e) => {
                    set_err_from(&e, &format!("read inode {ino}"));
                    return -1;
                }
            };
            if inode.is_dir() {
                set_err_msg(&format!("write_file {path_str}: is a directory"), EISDIR);
                return -1;
            }
            if !inode.is_file() {
                set_err_msg(
                    &format!("write_file {path_str}: not a regular file"),
                    EINVAL,
                );
                return -1;
            }
            let slice: &[u8] = if len == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(data as *const u8, len as usize)
            };
            match fs_ref.apply_replace_file_content(path_str, slice) {
                Ok(new_size) => new_size as i64,
                Err(e) => {
                    set_err_from(&e, &format!("write_file {path_str} ({len} bytes)"));
                    -1
                }
            }
        }),
    )
}

/// Mount an ext4 filesystem read-write. Companion to `fs_ext4_mount`.
/// Returns NULL on failure. A successful mount will replay a dirty journal
/// before returning.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_mount_rw(device_path: *const c_char) -> *mut fs_ext4_fs_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            clear_last_error();
            let path = cstr_to_str(device_path);
            if path.is_empty() {
                set_err_msg("null or empty device_path", EINVAL);
                return std::ptr::null_mut();
            }
            let dev = match FileDevice::open_rw(path) {
                Ok(d) => Arc::new(d) as Arc<dyn BlockDevice>,
                Err(e) => {
                    set_err_from(&e, &format!("open_rw {path}"));
                    return std::ptr::null_mut();
                }
            };
            match Filesystem::mount(dev) {
                Ok(fs) => Box::into_raw(Box::new(fs_ext4_fs_t { fs })),
                Err(e) => {
                    set_err_from(&e, &format!("mount_rw {path}"));
                    std::ptr::null_mut()
                }
            }
        }),
    )
}

/// Create a hard link at `dst` pointing at the same inode as `src`.
/// Source must not be a directory; dest must not already exist; dest's
/// parent must be a directory. Increments the shared inode's
/// `i_links_count`. Returns 0 on success, -1 on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_link(
    fs: *mut fs_ext4_fs_t,
    src: *const c_char,
    dst: *const c_char,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || src.is_null() || dst.is_null() {
                set_err_msg("null fs/src/dst", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let src_str = cstr_to_str(src);
            let dst_str = cstr_to_str(dst);
            match fs_ref.apply_link(src_str, dst_str) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("link {src_str} -> {dst_str}"));
                    -1
                }
            }
        }),
    )
}

/// Rename / move `src` → `dst` within this mount. Works for files and
/// directories; cross-parent moves fix the moved dir's `..` entry and
/// adjust both parents' `i_links_count`. Dest must NOT already exist —
/// overwrite-on-rename is a follow-up. Returns 0 on success, -1 on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_rename(
    fs: *mut fs_ext4_fs_t,
    src: *const c_char,
    dst: *const c_char,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || src.is_null() || dst.is_null() {
                set_err_msg("null fs/src/dst", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let src_str = cstr_to_str(src);
            let dst_str = cstr_to_str(dst);
            match fs_ref.apply_rename(src_str, dst_str) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("rename {src_str} -> {dst_str}"));
                    -1
                }
            }
        }),
    )
}

/// Create a subdirectory at `path` with POSIX permission bits `mode` (low
/// 12 bits used; file-type bits are set automatically). Returns the new
/// directory's inode number on success, 0 on failure.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_mkdir(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    mode: u16,
) -> u32 {
    ffi_guard(
        0u32,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return 0u32;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            match fs_ref.apply_mkdir(path_str, mode) {
                Ok(ino) => ino,
                Err(e) => {
                    set_err_from(&e, &format!("mkdir {path_str}"));
                    0u32
                }
            }
        }),
    )
}

/// Remove an empty directory at `path`. Fails if the directory contains
/// entries other than `.` and `..`. Returns 0 on success, -1 on failure
/// with details in `fs_ext4_last_error`.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_rmdir(fs: *mut fs_ext4_fs_t, path: *const c_char) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            match fs_ref.apply_rmdir(path_str) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("rmdir {path_str}"));
                    -1
                }
            }
        }),
    )
}

/// Change the permission bits on `path`. Only the low 12 bits of `mode`
/// (suid/sgid/sticky + rwx/rwx/rwx) are applied; file-type bits (`S_IFMT`)
/// are preserved from the existing inode. Bumps `i_ctime`.
///
/// Returns 0 on success, -1 on failure with details in `fs_ext4_last_error`.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_chmod(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    mode: u16,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            match fs_ref.apply_chmod(path_str, mode) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("chmod {path_str}"));
                    -1
                }
            }
        }),
    )
}

/// Change the owner of `path` to (`uid`, `gid`). Passing `u32::MAX` (0xFFFF_FFFF)
/// for either parameter leaves that value unchanged (matches Linux chown(2)
/// "-1 means leave alone" convention). Bumps `i_ctime`.
///
/// Returns 0 on success, -1 on failure with details in `fs_ext4_last_error`.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_chown(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    uid: u32,
    gid: u32,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            match fs_ref.apply_chown(path_str, uid, gid) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("chown {path_str}"));
                    -1
                }
            }
        }),
    )
}

/// Set the access + modification times on `path`. Each `*_sec` is the
/// POSIX seconds-since-epoch; pass `u32::MAX` (0xFFFF_FFFF) to leave a
/// given pair unchanged. `*_nsec` are the sub-second nanoseconds (written
/// only when i_extra_isize covers them). Bumps i_ctime.
///
/// Returns 0 on success, -1 on failure with details in
/// `fs_ext4_last_error`.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_utimens(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    atime_sec: u32,
    atime_nsec: u32,
    mtime_sec: u32,
    mtime_nsec: u32,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs/path", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            match fs_ref.apply_utimens(path_str, atime_sec, atime_nsec, mtime_sec, mtime_nsec) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("utimens {path_str}"));
                    -1
                }
            }
        }),
    )
}

/// Create a symbolic link at `linkpath` whose target is `target`. POSIX
/// `symlink(target, linkpath)` semantics: `target` is the arbitrary string
/// the symlink points to (can be absolute or relative, need not exist);
/// `linkpath` is the path where the symlink is created. Parent of
/// `linkpath` must exist and be a directory; `linkpath` itself must not
/// already exist.
///
/// v1 limit: `target` must be ≤ 60 bytes (fast-symlink path). Longer
/// targets return -1 with EINVAL + an explanatory `fs_ext4_last_error`.
///
/// Returns the new inode number on success (> 0), or 0 on failure with
/// details in `fs_ext4_last_error`.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_symlink(
    fs: *mut fs_ext4_fs_t,
    target: *const c_char,
    linkpath: *const c_char,
) -> u32 {
    ffi_guard(
        0u32,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || target.is_null() || linkpath.is_null() {
                set_err_msg("null fs/target/linkpath", EINVAL);
                return 0u32;
            }
            let fs_ref = &(*fs).fs;
            let target_str = cstr_to_str(target);
            let linkpath_str = cstr_to_str(linkpath);
            match fs_ref.apply_symlink(target_str, linkpath_str) {
                Ok(ino) => ino,
                Err(e) => {
                    set_err_from(&e, &format!("symlink {linkpath_str} -> {target_str}"));
                    0u32
                }
            }
        }),
    )
}

/// Remove the extended attribute `name` from the inode at `path`. `name`
/// must be fully-qualified (carry a known namespace prefix like `"user."`
/// or `"security."`). v1 scope: in-inode xattrs only; external-block
/// removal surfaces EINVAL until the slow path lands.
///
/// Returns 0 on success, -1 on failure with details in
/// `fs_ext4_last_error`. `fs_ext4_last_errno` codes: ENOENT if the name
/// isn't present, EINVAL on unknown prefix or external-block-only entry,
/// EROFS on a read-only mount.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_removexattr(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    name: *const c_char,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || name.is_null() {
                set_err_msg("null fs/path/name", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            let name_str = cstr_to_str(name);
            match fs_ref.apply_removexattr(path_str, name_str) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("removexattr {path_str} {name_str}"));
                    -1
                }
            }
        }),
    )
}

/// Set (create or replace) the extended attribute `name` on `path` with
/// `value_len` bytes from `value`. `name` must be fully-qualified
/// (carry a known namespace prefix like "user.").
///
/// v1 scope: in-inode xattrs only. ENOSPC if the in-inode region is
/// too small; external-block spill is not implemented.
///
/// Returns 0 on success, -1 on failure with details in
/// `fs_ext4_last_error`. `fs_ext4_last_errno` codes: EINVAL on unknown
/// prefix or null args, ENAMETOOLONG on >255-byte suffix, ENOSPC on
/// in-inode overflow, EROFS on RO mount.
#[no_mangle]
pub unsafe extern "C" fn fs_ext4_setxattr(
    fs: *mut fs_ext4_fs_t,
    path: *const c_char,
    name: *const c_char,
    value: *const c_void,
    value_len: usize,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || name.is_null() {
                set_err_msg("null fs/path/name", EINVAL);
                return -1;
            }
            if value.is_null() && value_len > 0 {
                set_err_msg("null value with nonzero len", EINVAL);
                return -1;
            }
            let fs_ref = &(*fs).fs;
            let path_str = cstr_to_str(path);
            let name_str = cstr_to_str(name);
            let value_bytes = if value_len == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(value as *const u8, value_len)
            };
            match fs_ref.apply_setxattr(path_str, name_str, value_bytes) {
                Ok(()) => 0,
                Err(e) => {
                    set_err_from(&e, &format!("setxattr {path_str} {name_str}"));
                    -1
                }
            }
        }),
    )
}
