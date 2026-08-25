# Working in rust-fs-ext4 (agent guide)

Pure-Rust ext2/3/4 driver (`fs-ext4`) exposing a C ABI (`fs_ext4_*`), consumed
by the DiskJockey FSKit extension. This file is the fast path for an agent
adding or fixing functionality, so you don't re-derive the workflow each time.
It points at the existing docs rather than duplicating them:

- **README** → `## Test contract` (suite shape), `## Building`, `### Testing`.
- **docs/TEST-DISKS.md** → the `test-disks/*.img` fixtures + their `.meta.txt`.

## Skills to use

- **`dev-loop`** — the required loop for any non-trivial change: baseline the
  full suite → change → re-run (no baseline test may regress) → enhance tests →
  vet. Always run it.
- **`commit`** / **`pr`** — for grouping commits and opening PRs. Branches are
  `cth/<name>`; commit subject + flat one-sentence bullets; **no AI attribution**.
- Discipline for **bug fixes**: **prove it's broken first** (a failing
  check/test), *then* fix, *then* prove the same check is green, *then* confirm
  the full baseline still passes. Never write the fix before you have a red.

## Running tests

```sh
cargo test                  # full suite (lib + integration). ~700 tests.
cargo test --test <name>    # one integration binary, e.g. repro_wants_dir_symlinks
cargo clippy --all-targets -- -D warnings   # what the pre-commit hook runs
```

Install the hooks once per clone: `./scripts/install-hooks.sh` (runs
`cargo fmt --check` + `cargo clippy -D warnings` on every commit).

## Adding a test (the in-tree pattern)

Integration tests copy a fixture, drive the driver via `apply_*`, then reopen
read-only and assert. Canonical templates:
`tests/journal_writer_create_mkdir_link_symlink.rs`,
`tests/repro_wants_dir_symlinks.rs`.

```rust
let path = copy_to_tmp("ext4-csum-seed.img", "tag");      // fixture → unique /tmp copy
{
    let dev = FileDevice::open_rw(&path)?;
    let fs = Filesystem::mount(Arc::new(dev))?;           // replays journal if dirty
    fs.apply_mkdir("/d", 0o755)?;
    fs.apply_symlink("../x", "/d/x")?;
    fs.apply_unlink("/d/x")?;
}                                                          // drop → unmount/flush
// reopen read-only and assert: jbd2::read_superblock().is_clean(),
// path::lookup(...), and — for checksum work — recompute & compare the
// on-disk checksum (see assert_jsb_checksum_valid in repro_wants_dir_symlinks.rs).
```

**`is_clean()` is not sufficient on its own** — it only checks `jsb.start == 0`.
A bad-but-marked-clean checksum passes it. For checksum bugs, either recompute
the specific checksum in-process (jsb example above) or cross-check with a real
ext4 (below).

## The oracle VM

`e2fsck`, `mkfs.ext4` and the in-kernel ext4 driver are Linux-only, so
on macOS they live in a Debian arm64 VM under QEMU with HVF — hardware
accelerated, unlike the x86_64 Alpine guest it replaced, which meant
full CPU emulation for every check on Apple Silicon.

```sh
./scripts/vm.sh up                       # boot; idempotent
./scripts/vm-e2fsck.sh <image>...        # e2fsck -fn each image
./scripts/vm.sh run <cmd>                # arbitrary command in the guest
./scripts/vm.sh down                     # halt; next `up` is fast
```

The VM stays up between invocations on purpose: booting is the slow
part, so an iterate-and-check loop should pay it once. A check costs
about 6 seconds once the VM is warm.

Two traps, both already paid for by the sibling drivers:

1. **Never set `config.notify_forwarder.enable = false`.** The plugin's
   `up` hook truncates the QEMU boot chain — the VM imports and then
   never boots, printing no error at all.
2. **`qe.virtiofs_guest_uid`/`gid` must match the box's `vagrant` user**
   (1001 here, not the plugin's 1000 default), or the shared folder is
   read-only to the guest and every fixture write fails with a bare
   permission error.

Ports are per-crate so several oracle VMs can run at once: xfs 50122,
btrfs 50123, ext4 50124.

## Cross-validation: the real-ext4 oracle (use this for checksum/layout bugs)

The driver shares this crate's spec interpretation, so its own
`verify::verify` / `fsck::audit` (structural: link counts, dirents, free-count
drift) **cannot** catch metadata_csum / journal-checksum / `itable_unused` bugs.
A **real Linux `e2fsck`** can. The repo's verification options:

- **Alpine QEMU VM** (`test-disks/build-ext4-feature-images.sh`, `_vm-builder.sh`,
  cached under `.vm-cache/`) — real Linux `mke2fs` + `e2fsprogs`. **This is the
  oracle.** It builds the fixtures and can `e2fsck` any image. No host
  `e2fsprogs` needed; no Docker.
- `scripts/cross-validate-lwext4.sh` + `tests/lwext4_cross_validate.rs` —
  independent C impl, opt-in (`LWEXT4_DIR`); currently a **read-only skeleton**.
- `tests/{qemu,vagrant}/freebsd/` — a real kernel, but FreeBSD's ext4 validates
  JBD2 differently from Linux; not pre-built.

### Recipe: `e2fsck` a driver-mutated image in the Alpine VM

`scripts/vm-e2fsck.sh <image>...` does this end-to-end. The mechanics + the
traps that cost time:

```sh
# boot (server mode): SSH on localhost:2222, key .vm-cache/builder-key,
# the dir in HOST_IMAGE_DIR is 9p-shared at /host inside the VM.
HOST_IMAGE_DIR=/path/to/dir bash test-disks/build-ext4-feature-images.sh --server > boot.log 2>&1
source test-disks/.vm-cache/server.env   # EXT4_BUILDER_PORT / _KEY / _PID
ssh -i "$EXT4_BUILDER_KEY" -p "$EXT4_BUILDER_PORT" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o BatchMode=yes \
    -o ServerAliveInterval=5 -o ServerAliveCountMax=2 root@localhost \
    'e2fsck -fn /host/<image>' </dev/null
kill "$EXT4_BUILDER_PID"                  # teardown
```

Traps (all hit during the metadata_csum fix series):
- **Do NOT pipe the boot script through `tail`/`head`** — qemu inherits its
  stdout, so the pipe reader never sees EOF and hangs. Redirect to a file.
- The shell here is **zsh**: `$SSH` as a string does **not** word-split — call
  `ssh` directly with an args array, or it becomes one "command not found" arg.
- Wrap each ssh in `timeout` + use `ServerAlive*`: `ConnectTimeout` bounds only
  the TCP connect, not a post-connect stall.
- e2fsck exit: `0` clean, `4` errors-left-uncorrected, `8` op error, `12`
  "cannot proceed" (e.g. corrupt journal superblock).

## Build environment

- The build cache (`target/`) is large. If the disk is tight, set
  `CARGO_TARGET_DIR` to a roomier volume — do **not** clean unrelated projects'
  `target/` dirs.
- Toolchain is pinned (`rust-toolchain.toml`); the hook enforces
  `clippy -D warnings` (e.g. `manual_div_ceil` → use `.div_ceil(n)`).

## Worked example: the metadata_csum write-path fixes

A real bug surfaced from the field (a Bookworm SD card whose journal the kernel
rejected after a write). Reproduced with `tests/repro_wants_dir_symlinks.rs` on
`ext4-csum-seed.img`, then fixed as a stack, each step proven red→green with
the Alpine-VM `e2fsck` and the full baseline:

1. **jbd2 superblock checksum** not recomputed in `journal_writer::write_jsb`.
2. **`bg_itable_unused`** not maintained on inode alloc (`buffer_mark_inode_used`).
3. **inode/block bitmap checksums** not recomputed on bitmap change
   (`buffer_refresh_bitmap_csum` + the four bitmap-mutating ops).
4. **inode checksum** not `i_extra_isize`-aware in `compute_inode_checksum`
   (zeroed/freed inodes need the 16-bit lo-only form).

The pattern to copy: `metadata_csum` writes must recompute **every** affected
checksum (superblock, group descriptor, bitmaps, inode, journal) — and the only
reliable proof is a real `e2fsck`, not the driver's own readers.
