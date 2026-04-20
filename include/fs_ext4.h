/*
 * fs_ext4.h — C ABI for the ext4rs pure-Rust ext4 driver.
 *
 * Link against libfs_ext4.a and #include this header. UTF-8 paths,
 * NULL / -1 / 0 failure sentinels with thread-local error details
 * available via fs_ext4_last_error() / fs_ext4_last_errno().
 *
 * MIT License — see LICENSE
 */

#ifndef FS_EXT4_H
#define FS_EXT4_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <sys/types.h>   /* mode_t, uid_t, gid_t */

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a mounted ext4 filesystem */
typedef struct fs_ext4_fs fs_ext4_fs_t;

/* File type enumeration (matches ext4 dir entry types) */
typedef enum {
    FS_EXT4_FT_UNKNOWN  = 0,
    FS_EXT4_FT_REG_FILE = 1,
    FS_EXT4_FT_DIR      = 2,
    FS_EXT4_FT_CHRDEV   = 3,
    FS_EXT4_FT_BLKDEV   = 4,
    FS_EXT4_FT_FIFO     = 5,
    FS_EXT4_FT_SOCK     = 6,
    FS_EXT4_FT_SYMLINK  = 7,
} fs_ext4_file_type_t;

/* File/directory attributes */
typedef struct {
    uint32_t inode;
    uint16_t mode;          /* POSIX mode bits */
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    uint32_t atime;
    uint32_t mtime;
    uint32_t ctime;
    uint32_t crtime;        /* Creation time (ext4 extra) */
    uint16_t link_count;
    fs_ext4_file_type_t file_type;
} fs_ext4_attr_t;

/* Directory entry (returned during iteration) */
typedef struct {
    uint32_t inode;
    uint8_t  file_type;     /* fs_ext4_file_type_t */
    uint8_t  name_len;
    char     name[256];     /* null-terminated */
} fs_ext4_dirent_t;

/* Volume information */
typedef struct {
    char     volume_name[16];
    uint32_t block_size;
    uint64_t total_blocks;
    uint64_t free_blocks;
    uint32_t total_inodes;
    uint32_t free_inodes;
    /*
     * 1 if the filesystem was NOT cleanly unmounted last time it was used
     * (captured from the on-disk s_state field at mount time, before any
     * journal replay the driver may perform). 0 if clean. Consumers should
     * surface a dirty value to the user and run fsck / journal replay
     * before permitting writes.
     */
    uint8_t  mounted_dirty;
} fs_ext4_volume_info_t;

/* ---- Block device callback interface ---- */

/*
 * Callback for reading blocks from the device.
 * Must read exactly `length` bytes at `offset` into `buf`.
 * Returns 0 on success, non-zero on error.
 * `context` is the opaque pointer passed to fs_ext4_mount_with_callbacks.
 */
typedef int (*fs_ext4_read_fn)(void *context, void *buf,
                                   uint64_t offset, uint64_t length);

/*
 * Block device parameters for callback-based mounting.
 */
typedef struct {
    fs_ext4_read_fn read;
    void   *context;     /* Passed to callbacks (e.g. FSBlockDeviceResource pointer) */
    uint64_t size_bytes; /* Total device/partition size */
    uint32_t block_size; /* Physical block size (e.g. 512) */
} fs_ext4_blockdev_cfg_t;

/* ---- Lifecycle ---- */

/*
 * Mount an ext4 filesystem from the given device/image path.
 * Uses direct POSIX I/O. Returns NULL on failure. Read-only.
 */
fs_ext4_fs_t *fs_ext4_mount(const char *device_path);

/*
 * Mount an ext4 filesystem using callback-based I/O.
 * Use this from sandboxed environments (e.g. FSKit extensions)
 * where direct device access is not available.
 * Returns NULL on failure. Read-only.
 */
fs_ext4_fs_t *fs_ext4_mount_with_callbacks(
    const fs_ext4_blockdev_cfg_t *cfg);

/*
 * Unmount and free all resources.
 */
void fs_ext4_umount(fs_ext4_fs_t *fs);

/* ---- Volume info ---- */

/*
 * Get volume information (name, sizes, free space).
 * Returns 0 on success.
 */
int fs_ext4_get_volume_info(fs_ext4_fs_t *fs,
                                fs_ext4_volume_info_t *info);

/* ---- File attributes ---- */

/*
 * Get attributes for a path (relative to mount root).
 * path should start with "/" e.g. "/etc/passwd"
 * Returns 0 on success.
 */
int fs_ext4_stat(fs_ext4_fs_t *fs, const char *path,
                     fs_ext4_attr_t *attr);

/* ---- Directory listing ---- */

/*
 * Directory iterator — opaque handle.
 */
typedef struct fs_ext4_dir_iter fs_ext4_dir_iter_t;

/*
 * Open a directory for iteration.
 * Returns NULL on failure.
 */
fs_ext4_dir_iter_t *fs_ext4_dir_open(fs_ext4_fs_t *fs,
                                              const char *path);

/*
 * Get the next directory entry.
 * Returns pointer to internal dirent (valid until next call or close).
 * Returns NULL when no more entries.
 */
const fs_ext4_dirent_t *fs_ext4_dir_next(fs_ext4_dir_iter_t *iter);

/*
 * Close directory iterator.
 */
void fs_ext4_dir_close(fs_ext4_dir_iter_t *iter);

/* ---- File reading ---- */

/*
 * Read file contents.
 * Returns bytes read, or -1 on error.
 */
int64_t fs_ext4_read_file(fs_ext4_fs_t *fs, const char *path,
                              void *buf, uint64_t offset, uint64_t length);

/* ---- Symlink ---- */

/*
 * Read symlink target.
 * Writes null-terminated target into buf (max bufsize bytes).
 * Returns 0 on success.
 */
int fs_ext4_readlink(fs_ext4_fs_t *fs, const char *path,
                         char *buf, size_t bufsize);

/* ---- Extended attributes ---- */

/*
 * List extended attribute names for a path.
 *
 * Writes NUL-separated fully-qualified names (e.g. "user.color\0user.tag\0")
 * into buf. If buf is NULL or bufsize is 0, no bytes are written but the
 * return value still reports the required total size — use this to probe.
 *
 * Returns: total bytes of output (names + NUL terminators) on success,
 *          -1 on error. If bufsize is less than the required size, writes
 *          as much as fits and still returns the required size.
 */
int64_t fs_ext4_listxattr(fs_ext4_fs_t *fs, const char *path,
                              char *buf, size_t bufsize);

/*
 * Get a single extended attribute value by fully-qualified name
 * (e.g. "user.color", "system.posix_acl_access").
 *
 * Writes raw value bytes (no NUL-terminator) into buf. If buf is NULL or
 * bufsize is 0, returns the value size without writing — use this to probe.
 *
 * Returns: value size in bytes on success,
 *          -1 if the name is not present or on error. If bufsize is less
 *          than the value size, writes as much as fits and still returns
 *          the value size.
 */
int64_t fs_ext4_getxattr(fs_ext4_fs_t *fs, const char *path,
                             const char *name, void *buf, size_t bufsize);

/* ---- Error reporting ---- */

/*
 * Get last error message (thread-local).
 * Returns pointer to static/thread-local string.
 */
const char *fs_ext4_last_error(void);

/*
 * Get POSIX errno for the last failed FFI call (thread-local).
 * Returns 0 if the last call succeeded (or no call has been made yet).
 * Codes: ENOENT, EIO, ENOTDIR, EINVAL, ENOTSUP — or any errno surfaced
 * by the underlying I/O layer (e.g. EACCES from the block device).
 * Use this alongside fs_ext4_last_error() to produce an NSError
 * with the correct POSIXErrorDomain code for FSKit.
 */
int fs_ext4_last_errno(void);

/*
 * ----- Write path (Phase 4, in progress) ------------------------------
 *
 * These exports require a read-write mount. Use fs_ext4_mount_rw()
 * for file-backed images; the existing callback mount is read-only.
 * On failure, -1 is returned and fs_ext4_last_error / _last_errno
 * describe the cause.
 */

/* Mount an ext4 filesystem read-write. Returns NULL on failure. A dirty
 * JBD2 journal is replayed before returning so callers see a consistent
 * on-disk state. */
fs_ext4_fs_t *fs_ext4_mount_rw(const char *device_path);

/* Shrink a regular file to `new_size` bytes. Frees the tail extents and
 * updates the inode size + blocks counter. Not yet journaled — safe only
 * on scratch images until the transaction wrapping lands. */
int fs_ext4_truncate(fs_ext4_fs_t *fs, const char *path,
                         uint64_t new_size);

/* Unlink a non-directory file at `path`. Decrements i_links_count; when
 * the count reaches zero the inode's extents are freed, its bitmap bit
 * cleared, and its body zeroed (with i_dtime = now). Refuses directories.
 * Returns 0 on success, -1 on failure. Not yet journaled. */
int fs_ext4_unlink(fs_ext4_fs_t *fs, const char *path);

/* Create a new empty regular file at `path` with the given permission
 * bits (e.g. 0644). Parent must exist and be a directory; the path must
 * not already exist. Returns the allocated inode number on success
 * (> 0), or 0 on failure. Not yet journaled. */
uint32_t fs_ext4_create(fs_ext4_fs_t *fs, const char *path,
                            uint16_t mode);

/* Replace the contents of an existing regular file with `len` bytes from
 * `data`. Frees any previous extents, allocates one contiguous run for
 * the new data, updates size + mtime + ctime. Returns the new size on
 * success, or -1 on failure. Not yet journaled; appends / partial writes
 * are follow-up work. */
int64_t fs_ext4_write_file(fs_ext4_fs_t *fs, const char *path,
                               const void *data, uint64_t len);

/* Rename / move `src` to `dst` within this filesystem. Supports files
 * and directories; cross-parent dir moves fix `..` + parent link counts.
 * Dest must not already exist. Returns 0 on success, -1 on failure.
 * Not yet journaled. */
int fs_ext4_rename(fs_ext4_fs_t *fs, const char *src,
                       const char *dst);

/* Create a hard link at `dst` pointing to the same inode as `src`.
 * Forbidden on directories. Dest must not already exist. Bumps the
 * shared inode's i_links_count by 1. Returns 0 on success, -1 on
 * failure. Not yet journaled. */
int fs_ext4_link(fs_ext4_fs_t *fs, const char *src,
                     const char *dst);

/* Create a subdirectory at `path` with POSIX permission bits `mode`
 * (typically 0755; low 12 bits used). Parent must exist and be a
 * directory; the path must not already exist. Seeds the new dir with
 * `.` and `..` entries and bumps the parent's i_links_count.
 * Returns the new directory's inode number on success (> 0), or 0 on
 * failure. Not yet journaled. */
uint32_t fs_ext4_mkdir(fs_ext4_fs_t *fs, const char *path,
                           uint16_t mode);

/* Remove an empty directory at `path`. Fails if the target contains
 * entries other than `.` and `..`. Frees its data blocks and inode,
 * removes the entry from the parent, and decrements the parent's
 * i_links_count. Returns 0 on success, -1 on failure. Not yet
 * journaled. */
int fs_ext4_rmdir(fs_ext4_fs_t *fs, const char *path);

/* Change the permission bits on `path`. Only the low 12 bits of `mode`
 * (suid/sgid/sticky + rwx/rwx/rwx) are applied; file-type bits are
 * preserved. Bumps i_ctime. Returns 0 on success, -1 on failure. */
int fs_ext4_chmod(fs_ext4_fs_t *fs, const char *path, uint16_t mode);

/* Change the owner of `path` to (`uid`, `gid`). Passing UINT32_MAX
 * (0xFFFFFFFF) for either parameter leaves that value unchanged
 * (matches Linux chown(2) semantics). Bumps i_ctime. Returns 0 on
 * success, -1 on failure. */
int fs_ext4_chown(fs_ext4_fs_t *fs, const char *path,
                  uint32_t uid, uint32_t gid);

/* Create a symbolic link at `linkpath` whose target is `target`.
 * POSIX symlink(target, linkpath) semantics. v1: fast-symlink only
 * (target ≤ 60 bytes); longer targets return 0 with EINVAL. Returns
 * the new symlink's inode number on success, 0 on failure. */
uint32_t fs_ext4_symlink(fs_ext4_fs_t *fs, const char *target,
                         const char *linkpath);

/* Set (create or replace) the extended attribute `name` on `path` with
 * `value_len` bytes from `value`. `name` must be fully-qualified
 * (e.g. "user.com.apple.FinderInfo"). v1: in-inode xattrs only;
 * ENOSPC if the in-inode region is too small; external-block spill
 * not implemented. Returns 0 on success, -1 on failure. */
int fs_ext4_setxattr(fs_ext4_fs_t *fs, const char *path,
                     const char *name, const void *value,
                     size_t value_len);

/* Remove the extended attribute `name` from the inode at `path`.
 * `name` must be fully-qualified (carry a known namespace prefix like
 * "user." or "security."). v1: in-inode xattrs only; external-block
 * removal surfaces EINVAL until the slow path lands. ENOENT when the
 * name isn't present. Returns 0 on success, -1 on failure. */
int fs_ext4_removexattr(fs_ext4_fs_t *fs, const char *path,
                        const char *name);

/* Set the access + modification times on `path`. Each `*_sec` is a
 * POSIX seconds-since-epoch value; passing UINT32_MAX leaves that pair
 * unchanged (so `atime_sec == UINT32_MAX` touches only mtime, etc).
 * `*_nsec` is sub-second nanoseconds, only written when the inode's
 * i_extra_isize region can hold them. Bumps i_ctime. Returns 0 on
 * success, -1 on failure. */
int fs_ext4_utimens(fs_ext4_fs_t *fs, const char *path,
                    uint32_t atime_sec, uint32_t atime_nsec,
                    uint32_t mtime_sec, uint32_t mtime_nsec);

#ifdef __cplusplus
}
#endif

#endif /* FS_EXT4_H */
