# ext4rs

Pure-Rust ext4 filesystem driver. Exposes a stable C ABI (`fs_ext4_*`)
designed to be linked from C, C++, Go (via CGo), or any other
language with FFI. Portable cargo crate — no platform-specific
dependencies.

It is used in production by
[DiskJockey](https://github.com/christhomas/diskjockey), but carries
no coupling back to that project — any FFI host can consume it.

## Origins

ext4rs is an independent pure-Rust implementation of the ext4
on-disk format. It is **not** a dependency wrapper — it has no
runtime dependency on any other ext4 Rust crate or C library, only
on `crc32c` + `bitflags`. The code draws research reference from
[yuoo655/ext4_rs](https://github.com/yuoo655/ext4_rs) (MIT) and
[lwext4](https://github.com/gkostka/lwext4) (BSD), both credited in
the license section.

## What this adds over the research references

`yuoo655/ext4_rs` is a read-oriented port of lwext4 to Rust;
`lwext4` is a C ext4 library intended for embedded use. Neither
exposes a stable Rust-native C ABI shaped around generic FFI
consumers, and both have gaps around mutation coverage + host
integration. ext4rs keeps the on-disk knowledge but reshapes the
surface:

| Area | ext4rs | yuoo655/ext4_rs | lwext4 |
|---|---|---|---|
| Pure Rust, no `unsafe` in read path | ✓ | ✓ | n/a (C) |
| Stable C ABI (`fs_ext4_*`, `fs_ext4.h`) | ✓ | ✗ | ✓ (different ABI) |
| Thread-local error state with inferred errno | ✓ | ✗ | partial |
| `mount_with_callbacks` (block-device callback transport) | ✓ | ✗ | ✗ |
| Opt-in LRU block cache for remote/callback devices | ✓ (`CachingDevice`) | ✗ | ✗ |
| Extent tree mutation (depth 0→1 promotion, depth-1 inserts) | ✓ | partial | ✓ |
| `metadata_csum` + `csum_seed` verification on read *and* write | ✓ | partial | ✓ |
| htree directory reads + writes | ✓ | ✓ | ✓ |
| `rename` with POSIX semantics (`EEXIST`, `EINVAL` into own subtree) | ✓ | partial | partial |
| `setxattr` / `removexattr` (in-inode) | ✓ | ✗ | ✓ |
| `chmod` / `chown` / `utimens` via C ABI | ✓ | ✗ | ✓ |
| Inline-data files (read + write) | ✓ | ✓ | ✓ |
| JBD2 journal replay on mount | ✓ | ✗ | partial |
| Read-only audit (`Filesystem::audit`, link counts, dangling entries) | ✓ | ✗ | ✗ |
| Integration tests against `mkfs.ext4` reference images (Alpine VM) | ✓ | ✗ | ✗ |

Scope notes and known gaps are listed in the status table below.

## Status

| Capability | Status |
|---|---|
| Mount ext4 image or block device | done |
| stat, readdir, read file | done |
| readlink, listxattr, getxattr | done |
| inline data, htree directories | done |
| extent trees (leaf + single-level internal) | done |
| checksum verification (metadata_csum, csum_seed) | done |
| create, unlink, mkdir, rmdir | done |
| rename (no-clobber, POSIX `EINVAL` into own subtree) | done |
| hardlink, truncate (shrink), write file (replace body) | done |
| multi-level extent tree mutation (depth 0→1 promotion + depth-1 inserts) | done |
| multi-level extent tree mutation (depth ≥2, leaf-block split) | **not supported** |
| sparse grow via truncate (i_size bump, reads return zeros) | done |
| setxattr, removexattr (in-inode) | done (via `fs_ext4_setxattr` / `fs_ext4_removexattr`) |
| setxattr/removexattr on external xattr block | **not supported** |
| chmod, chown, utimens | done (via `fs_ext4_chmod` / `fs_ext4_chown` / `fs_ext4_utimens`) |
| journaled transactions | partial (jbd2 replay; write path unjournaled) |
| read-only fsck audit (link counts, dangling entries) | done (via `Filesystem::audit`) |
| opt-in LRU block cache (for remote callback devices) | done (via `CachingDevice`) |

Roughly a read/write driver for the common case. Directories that have
been promoted to depth 1 can keep growing up to their leaf block's
capacity (~340 extents on a 4 KiB block); beyond that, or for sparse-
file extension, are the known gaps. POSIX errnos are mapped through
(`ENOENT`, `ENOTDIR`, `EISDIR`, `EINVAL`, `EEXIST`, `ENOTEMPTY`,
`EROFS`, `ENAMETOOLONG`, `ENOTSUP`).

## Building

```sh
cargo build --release
# produces target/release/libfs_ext4.a and the rlib
```

Cross-compile to a specific target the usual way:

```sh
cargo build --release --target aarch64-unknown-linux-gnu
cargo build --release --target x86_64-pc-windows-gnu
# … etc.
```

Platform-specific packaging (e.g. macOS `lipo` to build a universal
static archive, producing an Xcode `.xcframework`, or cross-compile
matrices for a particular SDK) belongs in the consuming project.
`ext4rs` itself stays portable cargo — it does not carry platform-
specific build scripts.

## Using from C

Link `libfs_ext4.a` and include `fs_ext4.h`:

```c
#include "fs_ext4.h"

fs_ext4_fs_t *fs = fs_ext4_mount("/path/to/disk.img");
if (!fs) {
    fprintf(stderr, "%s\n", fs_ext4_last_error());
    return 1;
}

fs_ext4_attr_t attr;
if (fs_ext4_stat(fs, "/hello.txt", &attr) == 0) {
    printf("size=%llu mode=%o\n", attr.size, attr.mode);
}

fs_ext4_umount(fs);
```

See `examples/capi_demo.rs` for the Rust-side equivalent.

## Using from Rust

```toml
[dependencies]
ext4rs = "0.1"
```

```rust
use fs_ext4::Filesystem;

let fs = Filesystem::mount("/path/to/disk.img")?;
let attrs = fs.stat("/hello.txt")?;
```

## Testing

```sh
cargo test --release
```

Integration tests use ext4 image fixtures under `test-disks/`.
The fixtures are gitignored — regenerate them with:

```sh
bash test-disks/build-ext4-feature-images.sh
```

The generator runs `mkfs.ext4` + `setfattr` + `setfacl` inside a
short-lived Alpine Linux VM booted under `qemu-system-x86_64`, so
the same script works on macOS, Linux, and in CI (no docker
required). First run downloads the Alpine virt ISO + kernel (~75 MB,
cached under `test-disks/.vm-cache/`).

## License

MIT — see [LICENSE](LICENSE). Derives research from
[yuoo655/ext4_rs](https://github.com/yuoo655/ext4_rs) (MIT) and
[lwext4](https://github.com/gkostka/lwext4) (BSD).

## Disclaimer — use at your own risk

**Read this before pointing the crate at anything you care about.**

This is experimental filesystem code that reads *and writes* the
on-disk structures of live filesystems. Bugs in this class of code
can — and sooner or later will — corrupt or destroy data. The MIT
license above already contains the standard no-warranty and
limitation-of-liability clauses; this section restates them in
plain English so there is no ambiguity about what you are agreeing
to when you use the software.

**By using this software you accept that:**

- The author(s) and contributors provide this crate **as is**, with
  **no warranty of any kind**, express or implied — including but
  not limited to warranties of merchantability, fitness for a
  particular purpose, correctness, data integrity, durability,
  security, or non-infringement.
- The author(s) and contributors are **not liable** for any loss,
  damage, or expense of any kind arising out of or related to your
  use of the software. This explicitly includes (non-exhaustively)
  lost or corrupted data, corrupted filesystems, volumes that will
  no longer mount, hardware damage, downtime, lost revenue, missed
  deadlines, support costs, or any direct, indirect, incidental,
  special, consequential, or punitive damages — regardless of the
  legal theory under which such damages might be sought.
- You are **solely responsible** for backing up any data that could
  be touched by this software *before* running it. The only safe
  workflow when experimenting with an unofficial filesystem driver
  is: work on disk *images* or on *copies*, never on your only
  copy of anything irreplaceable.
- If that is not acceptable to you, **do not use this software**.

This disclaimer is a plain-English restatement of the license terms
above, not a separate license. The license terms apply in full.
