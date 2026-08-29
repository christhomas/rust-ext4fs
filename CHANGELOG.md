# Changelog

## [Unreleased]

## [0.4.1] — 2026-08-29

### Fixes

- **`mkfs.ext4 -c` no longer swallows the device path** — `-c` asks the standard
  formatter for a bad-block scan and takes no argument, but it was listed among
  the flags that consume the token after them. `mkfs.ext4 -c disk.img` therefore
  read the image path as the value of `-c` and then failed with "missing
  positional `<device>` argument" about the path it had just consumed.

- **`mkfs.ext4 -b` rejects an unusable block size before opening the device** —
  the power-of-two / 1 KiB..64 KiB rule was enforced only inside the formatter,
  which runs after the device is opened read-write and after the tool has
  printed that it is formatting it, so `-b 3000` looked like a format that had
  failed part way through rather than a rejected argument. The rule now lives in
  one predicate (`mkfs::is_valid_block_size`) that both the formatter and the
  CLI call.

- **`mkfs.ext4 -q` no longer depends on where it appears** — `quiet` was read
  while the loop that sets it was still running, so `-q -m 1` was silent and
  `-m 1 -q` was not.

### Testing

- The `mkfs_e2fsck_oracle` suite gains multi-group cases (320 MiB / three
  groups with a short final group, and 640 MiB / five groups so a backup lands
  in group 3 under the sparse-super powers-of-3 rule), and CI runs `fsck.ext4`
  against both — including through their backup superblocks with `-b`. The
  multi-group formatter path had previously been validated only by this crate's
  own reader. It passes.

## [0.4.0] — 2026-06-30

### Features

- **Multiple block groups (ext4)** — `format_filesystem` /
  `format_filesystem_with_flavor` formats ext4 volumes spanning more than one
  block group: a full block-group descriptor table plus per-group block and
  inode bitmaps and inode tables. Superblock/GDT backups are written in the
  `RO_COMPAT_SPARSE_SUPER` groups (0, 1, and further powers of 3/5/7), so that a
  damaged primary superblock is recoverable via `e2fsck -b`. Only the filesystem
  metadata is written, so large volumes format quickly. ext2/ext3 remain
  single-group entities.

### Fixes

- **Metadata-checksum & free-count coherency on write** — bitmap, inode, and
  journal-superblock checksums are now recomputed when the underlying metadata
  changes; `bg_itable_unused` is maintained on inode allocation; superblock
  free-block / free-inode counts stay coherent across consecutive allocate/free
  ops (`patch_sb_counters` was re-reading the immutable mount-time snapshot and
  clobbering deltas). Fixes `e2fsck` "does not match checksum" / "free … count
  wrong" reports on written images.

- **Write-path hardening (dir / xattr / pwrite)** — `apply_rmdir` clears the
  freed directory inode; external xattr blocks compute correct entry/block
  hashes; `removexattr`'s external-block free is journaled (crash-safe); large
  pwrites are split into per-transaction chunks so they can't overflow a single
  journal descriptor block. Directory growth refreshes the block-bitmap
  checksum alongside the BGD + SB counter updates in one cache-coherent commit.

- **mkfs 1 KiB-block & ext3 correctness** — the group-0 block bitmap is indexed
  in bit space (accounting for `first_data_block = 1` on 1 KiB blocks), fixing
  an off-by-one tail and the missing trailing pad bit; the ext3 journal's
  indirect-map blocks are counted as used. 1 KiB and ext3 images now pass the
  in-process `fsck::audit`; 2 KiB / 4 KiB output is byte-identical to before.
  New `mkfs_e2fsck_oracle` / `mkfs_ext3_oracle` oracle test harnesses across
  block sizes and flavors.

## [0.3.3] — 2026-06-21

- Renamed the published crate `fs-ext4` → `am-fs-ext4` (consistency with the
  `am-*` sister crates); first pipeline-driven release under the new name. The
  `[lib]` name stays `fs_ext4`, so the C ABI / `.a` / downstream linking are
  unchanged.

## [0.3.2] — 2026-06-09

- README accuracy pass (dependencies, test counts, version).

## [0.3.1] — 2026-06-09

- Release pipeline: drop `--locked` from `cargo publish` so it can rewrite the
  `am-fs-core` path dependency into its registry equivalent in the packaged
  lockfile.

## [0.3.0] — 2026-05-30

### Features

- **Directory extent trees beyond depth 1** — `extend_dir_and_add_entry` now
  handles directories whose extent trees have reached depth ≥ 2 (previously
  returned a hard error). Uses the same `plan_insert_extent_deep` machinery
  as file writes; allocates index-node blocks on demand via
  `plan_block_allocation`.

- **Depth-1 directory leaf-full fallback** — When the single leaf block in a
  depth-1 directory extent tree fills up, the driver now falls back to the
  deep path (adds a sibling leaf or promotes to depth 2) instead of returning
  an error.

- **Full Unicode NFD + case fold for CASEFOLD directories** — `fold_name` now
  applies proper Unicode NFD decomposition (`unicode-normalization` crate)
  followed by Unicode full case fold (`caseless` crate). Previously only
  ASCII A–Z → a–z was folded, causing lookups for non-ASCII names (e.g. 'ñ'
  vs 'Ñ', 'ß' vs 'ss') in CASEFOLD-enabled directories to miss the htree
  entry and silently fail to find the file.

- **`fs_ext4_mknod`** — New C API function to create special files: FIFOs,
  Unix domain sockets, character devices, and block devices. Mirrors POSIX
  `mknod(2)`; accepts `mode` with type bits (S_IFIFO=0x1000, S_IFSOCK=0xC000,
  S_IFCHR=0x2000, S_IFBLK=0x6000) plus permission bits, and `major`/`minor`
  device numbers (0 for FIFOs and sockets). Device numbers encoded using both
  old format (i_block[0]) and new format (i_block[1]) for maximum
  compatibility.

- **`fs_ext4_set_flags`** — New C API function to update the `i_flags` word
  (FS_IOC_SETFLAGS) on any path. Bumps i_ctime on write.

- **`fs_ext4_attr_t` extended** — Seven new fields appended at the end of the
  struct (ABI-compatible for consumers that zero-initialise):
  - `atime_nsec`, `mtime_nsec`, `ctime_nsec`, `crtime_nsec` — sub-second
    nanoseconds from the extra timestamp words; zero on ext2/ext3 inodes.
  - `inode_flags` — on-disk e2_flags (FS_IOC_GETFLAGS convention); callers
    can detect IMMUTABLE, NODUMP, APPEND_ONLY, CASEFOLD, etc.
  - `generation` — `i_generation` for NFS stale-handle detection.
  - `blocks_512` — `i_blocks` in 512-byte units (matches `st_blocks`).

## [0.2.1] — 2026-05-20

### Fixes

- **`fs_ext4_create` now writes `i_crtime`** at offset 0x90 across
  all four inode builders (regular file, directory, fast symlink,
  slow symlink). Previously the field was left at zero on freshly
  created inodes, surfacing on Darwin as `st_birthtime` = 0 and
  Finder showing "1 January 1970" as the file's "Created" date
  even though atime / ctime / mtime were set correctly. Gated on
  `inode_size >= 0x94` so ext2 / ext3 128-byte inodes (which lack
  the extra section) are left alone.

### Tests

- New `tests/capi_create.rs::create_sets_timestamps_to_now`
  regression test brackets all four timestamp fields against
  `SystemTime::now()` either side of `fs_ext4_create`. Fails on
  the pre-fix code.

### Release pipeline

- `cargo publish --locked` in the release workflow. Earlier
  publish attempts failed because `Cargo.lock` got modified
  during the implicit publish-verify build and the working tree
  went dirty; the lazy `--allow-dirty` would have hidden any real
  drift. With `--locked`, cargo refuses to update the lockfile
  during publish, so the version of every transitive dep
  committed at the tag is the version that ships. Discipline:
  any Cargo.toml `version` bump must be paired with
  `cargo update --workspace` + a lockfile commit in the same
  release prep.

## [0.2.0] — TODO

(Pre-existing release. Changelog entry not authored at the time;
fill in if it ever becomes useful.)

## [0.1.4] — TODO

(Pre-existing release. Changelog entry not authored at the time;
fill in if it ever becomes useful.)

## [0.1.3] — 2026-04-22

### Fixes

- `tests/capi_basic.rs::volume_info_flags_dirty_image` is now
  formatted per rustfmt. Shipping 0.1.2 with that line unformatted
  broke `cargo fmt --check` in CI, which in turn blocked clippy and
  test. No ABI / behaviour change.

### Tooling

- Pre-commit hook (`.githooks/pre-commit`) runs the fast CI subset —
  `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`
  — so the same class of miss can't slip through again. Enable with
  `./scripts/install-hooks.sh`.

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
  ext4 driver; no more `Swift` / `FSKit`-specific language in the
  API description.
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
  boots a short-lived Alpine Linux VM, runs ext4 formatter + friends
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
