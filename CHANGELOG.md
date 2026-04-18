# Changelog

## [Unreleased]

### Added

- Multi-level extent tree promotion (depth 0 → depth 1) in
  `extent_mut`, with `Checksummer::patch_extent_tail` so newly
  built leaf blocks carry a valid `ext4_extent_tail.et_checksum`.

### Changed

- Full `cargo fmt` sweep and `cargo clippy --all-targets -- -D warnings`
  pass; CI now gates on fmt + clippy + test (`macos-14` + `ubuntu-latest`).
- `CallbackDevice` fields migrated from inline `Box<dyn Fn(...) + Send + Sync>`
  to `ReadCb` / `WriteCb` / `FlushCb` type aliases for readability.

## [0.1.0] — 2026-04-18

First public release. Extracted from the internal ext4-fskit research
repo into a standalone crate.

### C ABI — `ext4rs_*`

- Lifecycle: `ext4rs_mount`, `ext4rs_mount_with_callbacks`,
  `ext4rs_mount_rw`, `ext4rs_umount`, `ext4rs_get_volume_info`.
- Metadata: `ext4rs_stat`, `ext4rs_last_error`, `ext4rs_last_errno`.
- Directories: `ext4rs_dir_open`, `ext4rs_dir_next`, `ext4rs_dir_close`.
- Files: `ext4rs_read_file`, `ext4rs_readlink`, `ext4rs_listxattr`,
  `ext4rs_getxattr`.
- Write ops: `ext4rs_create`, `ext4rs_unlink`, `ext4rs_mkdir`,
  `ext4rs_rmdir`, `ext4rs_rename`, `ext4rs_link`, `ext4rs_write_file`,
  `ext4rs_truncate`.

### Known gaps

- Multi-level extent tree mutation not implemented; large / fragmented
  writes will fail loudly.
- Sparse grow via truncate not implemented.
- `setxattr`, `removexattr`, `chmod`, `chown`, `utimens` — not in the
  ABI; reads only for xattrs.
- Write path is unjournaled. `jbd2` replay works at mount for a
  cleanly-closed journal; live transactions are not yet wrapped.

### Origin

- Imported from `github.com/christhomas/ext4-fskit@aaa63cf`.
- Previous C ABI name `ext4_bridge_*` renamed to `ext4rs_*` to match
  the crate identity and remove the "bridge" branding that referred
  back to the deprecated C/lwext4 shim.
