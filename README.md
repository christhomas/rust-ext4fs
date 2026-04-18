# ext4rs

Pure-Rust ext4 filesystem driver. Exposes a stable C ABI (`ext4rs_*`)
designed to be linked from Swift, C, C++, Go (via CGo), or any other
language with FFI.

Built to back [DiskJockey](https://github.com/christhomas/diskjockey)'s
macOS FSKit extension for ext4 mounts, but has no macOS or FSKit
dependency itself — the library is portable Rust.

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
| sparse grow via truncate | **not supported** |
| setxattr, removexattr | **not supported** |
| chmod, chown, utimens | **not supported** |
| journaled transactions | partial (jbd2 replay; write path unjournaled) |

Roughly a read/write driver for the common case. Directories that have
been promoted to depth 1 can keep growing up to their leaf block's
capacity (~340 extents on a 4 KiB block); beyond that, or for sparse-
file extension, are the known gaps. POSIX errnos are mapped through
(`ENOENT`, `ENOTDIR`, `EISDIR`, `EINVAL`, `EEXIST`, `ENOTEMPTY`,
`EROFS`, `ENAMETOOLONG`, `ENOTSUP`).

## Building

```sh
cargo build --release
# produces target/release/libext4rs.a and the rlib
```

Universal macOS static lib:

```sh
./build.sh          # builds both archs + lipos them into dist/libext4rs.a
```

The CI workflow on tagged releases packages a `.xcframework` for
Swift/Xcode consumers.

## Using from C / Swift

Link `libext4rs.a` and include `ext4rs.h`:

```c
#include "ext4rs.h"

ext4rs_fs_t *fs = ext4rs_mount("/path/to/disk.img");
if (!fs) {
    fprintf(stderr, "%s\n", ext4rs_last_error());
    return 1;
}

ext4rs_attr_t attr;
if (ext4rs_stat(fs, "/hello.txt", &attr) == 0) {
    printf("size=%llu mode=%o\n", attr.size, attr.mode);
}

ext4rs_umount(fs);
```

See `examples/capi_demo.rs` for the Rust-side equivalent.

## Using from Rust

```toml
[dependencies]
ext4rs = "0.1"
```

```rust
use ext4rs::Filesystem;

let fs = Filesystem::mount("/path/to/disk.img")?;
let attrs = fs.stat("/hello.txt")?;
```

## Testing

```sh
cargo test --release
```

Integration tests use ext4 image fixtures under `test-disks/`.
Regenerate them with `./test-disks/gen-test-disks.sh` (requires
`mke2fs`).

## License

MIT — see [LICENSE](LICENSE). Derives research from
[yuoo655/ext4_rs](https://github.com/yuoo655/ext4_rs) (MIT) and
[lwext4](https://github.com/gkostka/lwext4) (BSD).
