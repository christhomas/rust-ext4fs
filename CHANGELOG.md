# Changelog

## [0.1.2] — 2026-04-20

### ABI additions

- `fs_ext4_volume_info_t` gained a trailing `uint8_t mounted_dirty`
  field. `1` means the filesystem was not cleanly unmounted last time
  it was used (captured from the on-disk `s_state` superblock field at
  mount time); `0` means clean. Callers can surface this to the user
  and run fsck / journal replay before permitting writes. Existing
  consumers compiled against 0.1.1 remain source-compatible — the new
  field is appended and initialised to 0 via the existing struct-zero
  path in `fs_ext4_get_volume_info`.

### Rust API additions

- `Superblock` now parses `s_state` into a new `state: u16` field and
  exposes `Superblock::is_clean()`. New constants `EXT4_VALID_FS` and
  `EXT4_ERROR_FS` mirror the kernel's `s_state` bits.

### Tests

- `tests/capi_basic.rs::volume_info_flags_dirty_image` flips `s_state`
  on a copy of the no-csum fixture and asserts the ABI surfaces
  `mounted_dirty == 1`. `volume_info_reports_expected_fields` now also
  asserts `mounted_dirty == 0` for the freshly-built clean fixture.

## [0.1.1] — 2026-04-20

### Docs / packaging

- README fully rewritten. New sections: origins, a concrete
  capability matrix contrasting ext4rs with its research references
  (`yuoo655/ext4_rs` and `lwext4`) to justify this crate's existence
  as an independent FFI-first implementation, and a plain-English
  at-your-own-risk disclaimer restating the MIT no-warranty clause.
- Framing neutralised: crate is described as a general-purpose FFI
  ext4 driver. DiskJockey is mentioned once as a production user
  with an explicit no-coupling note; no more `Swift` / `FSKit`-
  specific language in the API description.
- `Cargo.toml` description updated to match (`FFI from C/C++/Go/etc.`
  instead of `Swift/C/Go/etc.`) and `version` bumped to `0.1.1`.

### Safety / robustness

- Mount path no longer panics on malformed images. Superblock parse
  rejects `blocks_per_group == 0`, `inodes_per_group == 0`,
  `inode_size == 0`, `inode_size > block_size`, and `log_block_size`
  above the spec-sane maximum. Block/inode arithmetic in
  `fs::read_block`, `fs::read_inode_raw`, `bgd::locate_inode`, and
  `extent::lookup` now uses `checked_mul`/`checked_add`; overflows
  surface as structured `Error::Corrupt` instead of silent wraps or
  div-by-zero panics.
- New `tests/fuzz_smoke.rs` harness: truncated / zero-filled /
  all-ones images, an xorshift PRNG seed fan, single-byte flips at
  sampled superblock+BGDT+inode-table+dir-block offsets, direct
  random-bytes feeding into `dir::parse_block` and the extent
  parsers, and an exhaustive-single-bit-flip sweep of the
  superblock sector. Every combination must either succeed or
  return a structured `Err` — never panic.

### Features

- `Filesystem::audit(max_dirs, max_entries_per_dir)` — read-only
  fsck-style link-count audit (see `src/fsck.rs`). Returns an
  `AuditReport` listing `LinkCountTooLow` / `LinkCountTooHigh` /
  `DanglingEntry` / `WrongDotDot` / `BogusEntry` anomalies. Pure
  diagnostic: never writes. Bounded work so pathological images
  can't hang the caller.
- `CachingDevice` — LRU read cache decorator for any
  `Arc<dyn BlockDevice>`. Caches only block-aligned, block-sized
  reads (hot paths: `fs::read_block`, extent index blocks, bitmap
  blocks); passes arbitrary-offset reads through. Writes
  invalidate overlapping entries. Opt-in — existing callers see no
  behaviour change. Primary target is the FSKit `CallbackDevice`
  path where repeated reads of the same inode-table / bgd blocks
  dominate directory walks.

### Performance

- `alloc::find_first_free` — scan the free-block bitmap a `u64` at
  a time once aligned to an 8-byte word. Skips full words in a
  single branch; uses `trailing_ones` to locate the first zero
  within a non-full word. 8–16× faster than the previous per-bit
  loop on sparse bitmaps.

### Build / CI

- Test-disk fixtures now regenerate from scratch on any host with
  `qemu-system-x86_64` + `libarchive-tools` (for `bsdtar`'s
  ISO9660 writer). Drop-in `bash test-disks/build-ext4-feature-images.sh`
  boots a short-lived Alpine Linux VM, runs `mkfs.ext4` + friends
  inside, writes the image matrix out via 9p. Replaces the earlier
  docker-based path so macOS dev hosts don't need Docker Desktop.
  CI (`ubuntu-latest`) runs this before `cargo test`.

## [0.1.0] — 2026-04-18

First public release. Extracted from the internal ext4-fskit research
repo into a standalone crate.

### C ABI — `fs_ext4_*`

- Lifecycle: `fs_ext4_mount`, `fs_ext4_mount_with_callbacks`,
  `fs_ext4_mount_rw`, `fs_ext4_umount`, `fs_ext4_get_volume_info`.
- Metadata: `fs_ext4_stat`, `fs_ext4_last_error`, `fs_ext4_last_errno`.
- Directories: `fs_ext4_dir_open`, `fs_ext4_dir_next`, `fs_ext4_dir_close`.
- Files: `fs_ext4_read_file`, `fs_ext4_readlink`, `fs_ext4_listxattr`,
  `fs_ext4_getxattr`.
- Write ops: `fs_ext4_create`, `fs_ext4_unlink`, `fs_ext4_mkdir`,
  `fs_ext4_rmdir`, `fs_ext4_rename`, `fs_ext4_link`, `fs_ext4_write_file`,
  `fs_ext4_truncate`.

### Driver features

- Multi-level extent tree promotion (depth 0 → depth 1) in
  `extent_mut`, with `Checksummer::patch_extent_tail` so newly
  built leaf blocks carry a valid `ext4_extent_tail.et_checksum`.

### Build / CI

- `cargo fmt` + `cargo clippy --all-targets -- -D warnings` + `cargo
  test --release` on `ubuntu-latest`.
- `CallbackDevice` fields use `ReadCb` / `WriteCb` / `FlushCb` type
  aliases instead of inline `Box<dyn Fn(...) + Send + Sync>`.

### Known gaps

- Multi-level extent tree mutation beyond depth 1 not implemented;
  very large / fragmented writes will fail loudly.
- Sparse grow via truncate not implemented.
- `setxattr`, `removexattr`, `chmod`, `chown`, `utimens` — not in the
  ABI; reads only for xattrs.
- Write path is unjournaled. `jbd2` replay works at mount for a
  cleanly-closed journal; live transactions are not yet wrapped.

### Origin

- Imported from `github.com/christhomas/ext4-fskit@aaa63cf`.
