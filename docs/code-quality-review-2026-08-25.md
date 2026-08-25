# Code quality review — 2026-08-25

**Scope:** `src/`, 20,271 production lines across 38 files (test modules excluded from
every count below).
**Findings:** 3 high, 3 medium, 2 low. No fixes applied — this is a read of the code
as it stands.

This is the largest crate in the family and the one with the most write surface, and
the strain shows in one file. Nothing here is a correctness finding. Every item is
about how long it takes a reader to establish that the code is correct — which matters
more than usual in a crate that mutates filesystems.

---

## H1 — `fs.rs` is 4,920 lines holding 51 public functions in 2 `impl` blocks

**`src/fs.rs`**

A quarter of the crate in one file, and the file is named for the concept the crate is
about, which is what has let it absorb everything: there is no size at which "does this
belong in `fs.rs`?" answers itself.

It contains at least five separable concerns — mount and superblock handling, the
allocation planners' callers, the journalled write paths, directory mutation, and
truncate/fallocate. They are interleaved rather than grouped, so a reader following one
of them scrolls past the other four.

**Shape of the fix.** Split by operation family, keeping `Filesystem` as the type and
moving `impl Filesystem` blocks into `fs/write.rs`, `fs/dir_mut.rs`, `fs/truncate.rs`
and so on. Rust allows an inherent impl to be split across modules in the same crate,
so this is a move rather than a redesign.

---

## H2 — The directory-block checksum tail is written out three times

**`src/fs.rs:4440`, `:4577`, `:4722`**

```rust
if self.csum.enabled && reserved_tail == 12 {
    let end = block.len();
    block[end - 12..end - 8].copy_from_slice(&0u32.to_le_bytes());
    block[end - 8..end - 6].copy_from_slice(&12u16.to_le_bytes());
    block[end - 6] = 0;
    block[end - 5] = 0xDE;
    let mut c = crate::checksum::linux_crc32c(self.csum.seed, &parent_ino.to_le_bytes());
    c = crate::checksum::linux_crc32c(c, &parent_inode.generation.to_le_bytes());
    c = crate::checksum::linux_crc32c(c, &block[..end - 12]);
    block[end - 4..end].copy_from_slice(&c.to_le_bytes());
}
```

Three verbatim copies. This is the highest-severity duplication in the family, because
of *what* is duplicated: a checksum, built from a seed, two identity values and a byte
range, with a fake directory entry (`rec_len = 12`, `name_len = 0`, `file_type = 0xDE`)
laid down in front of it.

Every one of those constants has to agree with e2fsprogs, and nothing forces the three
copies to agree with each other. A fix applied to one — a corrected range, a changed
seed — leaves two copies silently writing checksums the kernel will reject, on a path
that produces no error until the volume is next mounted elsewhere.

The crate has 24 duplicated eight-line blocks and 51 occurrences overall; this is the
one that matters.

**Shape of the fix.** `fn write_dir_block_csum_tail(&self, block: &mut [u8], parent_ino: u32, generation: u32)`,
with the `0xDE` and the 12 named where they are defined rather than at each use.

---

## H3 — `apply_pwrite` (350 lines) and `apply_rename` (322 lines)

**`src/fs.rs:2835`, `src/fs.rs:3858`**

`apply_pwrite` labels its own structure:

```rust
// Phase 1: walk affected logical blocks; allocate each contiguous …
// Phase-2 writes for these MUST NOT read from disk (the prior …
```

As with the numbered sections in this family's other large functions, a function that
names its own phases has already been decomposed in the author's head. Here the phases
also have genuinely different concerns — Phase 1 is allocation policy including a
fragmentation retry loop that halves the request, Phase 2 is I/O with a
must-not-read-from-disk invariant that is stated in a comment and enforced by nothing.

That invariant is the strongest argument for extraction. `fn write_phase(&self, …, freshly_allocated: &BlockSet)`
can make it a property of the signature instead of a property of the reader's memory.

65 functions in the crate are 60 lines or longer; these two and
`format_filesystem_with_flavor` (325), `audit_inner` (293) and
`plan_insert_extent_deep` (212) lead the list.

---

## M4 — 271 lines indented 24 columns or deeper

**crate-wide**

Six levels of nesting and beyond — by a wide margin the most in the family (the next
worst is `rust-fs-erofs` at 56). The worst of it is inside the long functions above, in
the allocation retry loops, where a `for` inside a `while` inside a `match` inside an
`if` puts the interesting line eight levels from the margin.

Early returns and extracted helpers flatten most of this. It will largely resolve with
H3, but it is worth tracking separately because it is the metric that most directly
predicts how hard the code is to read.

---

## M5 — 55 functions take five or more parameters

**crate-wide**

The C ABI entries (`fs_ext4_read_file`, `fs_ext4_getxattr`) take what the ABI dictates
and should be left alone. The internal ones are mostly an unnamed struct: several
thread the same `(buf, gi, free_blocks_delta, free_inodes_delta, used_dirs_delta)`
group, and `acl::read` takes six.

Two of them already carry `#[allow(clippy::too_many_arguments)]`, which is the lint
correctly noticing and being told not to.

**Shape of the fix.** Name the recurring groups — a `BgdCounterUpdate` already exists
for exactly this and could absorb more call sites.

---

## M6 — 13 unnamed multi-digit offsets

**`src/htree.rs` (5), `src/extent_mut.rs` (2), `src/fs.rs` (2), `src/journal.rs` (1)**

Small in absolute terms and far better than `rust-fs-ntfs`'s 112, but they sit in the
htree and extent-tree code, which is the least forgiving place to have an offset a
reader cannot check by eye.

The crate mostly uses named offsets already, so this is a handful of lapses rather
than a missing convention.

---

## L7 — Seven `#[allow(...)]` with no stated reason

**crate-wide**

`#[allow(dead_code)]` and two `#[allow(clippy::too_many_arguments)]` appear without a
comment saying why the lint is wrong here. Each is probably justified; none says so.

---

## L8 — `capi.rs` is 2,867 lines

**`src/capi.rs`**

Large, but it is a flat list of ABI entry points with a consistent shape, which is the
one kind of long file that stays readable — a reader looking for one function finds it
by name and never needs the rest. Noted rather than recommended for change; splitting
it would add navigation cost without reducing what anyone has to hold in their head.

---

## What is good, and should survive any refactor

- **The comments explain *why*, and they explain the dangerous parts.** The
  must-not-read-from-disk invariant in `apply_pwrite`, the transaction tag-slot
  reservation, the fragmentation retry policy — all are documented at the point they
  matter.
- **Cross-validation is genuinely strong.** `fsck.ext4` and `lwext4` oracles, plus a
  Linux VM, mean the crate is checked against the implementations it has to agree
  with rather than against itself.
- **Named offsets are the norm**, with the small exceptions in M6.
- **`clippy -D warnings` and `rustfmt` are clean**, and CI enforces both.

## Suggested order

H2 first — it is the only finding with a plausible path to silently wrong bytes on
disk, and it is a twenty-minute change. Then H3, one phase at a time, which will take
M4 down with it. H1 last, since the file boundaries are easier to choose once the
functions inside them are smaller.
