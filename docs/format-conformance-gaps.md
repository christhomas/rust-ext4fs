# Format-conformance gaps

Where this implementation and the ext4 on-disk format disagree.

Two planning documents already exist and this is deliberately not a
third of the same kind:

- `ext4-full-write-support.md` — *features not yet built* (write paths,
  journalling, extent depth ≥ 2), organised as phases.
- `IMPROVEMENT-PLAN.md` — *code quality* (panics, error text, perf,
  test coverage).

This one is about a different axis: places where the crate **accepts a
filesystem it does not fully understand**, or reads a field in a way
the format does not sanction. The distinguishing feature of everything
below is that it fails *quietly* — a mount succeeds, and the wrongness
shows up later as bad data rather than as an error.

Started 2026-09-04. Findings are listed with the evidence that produced
them, so each can be re-checked rather than taken on trust.

---

## The general shape: tolerated is not implemented

`features.rs` keeps two masks — `SUPPORTED_INCOMPAT` and
`SUPPORTED_RO_COMPAT` — and refuses to mount anything carrying a bit
outside them. That is the right design. The gap is that several bits
are *inside* the masks with a comment conceding the feature is not
actually handled:

```rust
| Incompat::MMP.bits()          // ignore for read-only
| Incompat::INLINE_DATA.bits()  // we'll handle the flag, even if data overflow uses xattr later
| RoCompat::BIGALLOC.bits()     // tolerated; cluster math may need updates
```

A bit in the mask is a promise to the user that the filesystem will be
read correctly. Where that promise is not kept, the failure is silent.

**Two of those comments are now stale in the crate's favour** and
should be corrected so the list is trustworthy:

- `RECOVER` says "we'll skip journal replay for now (warn)" — journal
  replay exists (`Filesystem::replay_journal_if_dirty`, exported as
  `fs_ext4_replay_journal_if_dirty`).
- `CASEFOLD` is listed as merely tolerated — `src/casefold.rs`
  implements the UTF-8 casefolded hash against `fs/ext4/hash.c`.

---

## G1 — BIGALLOC is accepted and the cluster arithmetic is absent

**Severity: high.** Reads wrong data rather than failing.

`RoCompat::BIGALLOC` is in `SUPPORTED_RO_COMPAT`, with the comment
"cluster math may need updates". A search for cluster handling outside
`mkfs.rs` (which only asserts bigalloc is *off* when formatting) finds
none.

With bigalloc, the allocation unit becomes the **cluster**, not the
block: `s_log_cluster_size` exceeds `s_log_block_size`, block-group
bitmaps track clusters, and `s_clusters_per_group` replaces
`s_blocks_per_group` as the group stride. An implementation that
assumes cluster == block computes every block-group offset wrong on
such a filesystem, and reads whatever happens to live there.

**Where it bites:** block-group descriptor addressing, block-bitmap
interpretation, free-space accounting.

**Options, in order of honesty:**

1. Remove `BIGALLOC` from the mask so the mount is refused with a clear
   message. One line, and it converts silent corruption into an honest
   "unsupported". This is the right immediate fix.
2. Implement cluster arithmetic properly, then re-add the bit.

Do (1) now and (2) when someone needs it. mkfs.erofs-style images are
rare in the wild; being wrong about them silently is not worth the
compatibility claim.

---

## G2 — timestamps lose the epoch-extension bits after 2038

**Severity: medium.** Correct until 2038, then wrong.

`Inode` stores `atime`/`mtime`/`ctime`/`crtime` as `u32`, and the
`*_extra` fields are read only for their nanosecond half:

```rust
mtime_nsec = u32::from_le_bytes(raw[0x88..0x8C]...) >> 2;
```

The format puts the **seconds extension in the low two bits** of the
same field — the very bits discarded by `>> 2`. Those two bits widen
seconds from 32 to 34, moving the ceiling from 2038 to roughly 2446.
The crate's own doc comment on the field records the layout correctly
(“top 30 bits = nsec, low 2 bits = epoch”) and then does not use it.

There is a second, smaller point: the base fields are **signed** in the
format (`__le32` interpreted as `i32`), so timestamps before 1970 are
representable. Storing them as `u32` turns a 1969 date into a date in
2106.

**Fix:** store seconds as `i64`, apply the two epoch bits when
`i_extra_isize` is large enough to contain them, and keep `nsec`
separate.

**Note on scope.** This is *not* the bug found in `ext4-win-driver`,
which truncated seconds to `u32` in its own FILETIME conversion. That
one is a driver bug and is addressed by
`winfsp-fs-skeleton::translate`, which takes `i64` seconds. The two are
independent; fixing the driver does not fix this.

---

## G3 — INLINE_DATA is accepted with the overflow path unfinished

**Severity: medium.** The comment says it: "we'll handle the flag, even
if data overflow uses xattr later."

Inline data lives in the inode's `i_block` area, and **spills into the
`system.data` extended attribute** when it outgrows it. A reader that
handles only the `i_block` half returns a truncated file for any inline
file past the threshold — silently, because the size field still says
the full length.

**Fix:** either read the xattr continuation, or refuse a mount whose
inline files exceed the inline area. Worth checking first whether the
spill path is in fact implemented — the comment may be stale in the
crate's favour, as two others were.

---

## G4 — MMP is accepted and not honoured

**Severity: low for read-only, high if write support lands.**

Multi-Mount Protection exists to stop two hosts mounting one filesystem
simultaneously and destroying it. The bit is tolerated with "ignore for
read-only", which is defensible today: a read-only mount cannot corrupt
anything.

It stops being defensible the moment any write path is enabled on a
filesystem carrying this bit. Whoever lands write support needs to
either honour MMP (read the block, check the sequence, write our own
node name, re-check) or refuse to mount read-write when it is set.

**Recorded here so it is not discovered afterwards.**

---

## G5 — the supported-feature masks are not tested against real images

**Severity: process, not code.**

Every gap above was found by reading the masks and their comments, not
by a test failing. There is no test that formats a filesystem *with* a
given feature and asserts the crate either reads it correctly or
refuses it.

That is what would have caught G1 and G2 automatically, and it is the
one change that makes this document self-maintaining rather than a
snapshot.

**Shape:** for each bit in the masks, an image built with that feature
enabled (mkfs flags are known for all of them), and an assertion of the
intended behaviour — full read, or a clean refusal. `TEST-DISKS.md`
already describes the image-building infrastructure.

---

## Suggested order

| | gap | why this position |
|---|---|---|
| 1 | **G1** — drop BIGALLOC from the mask | one line; converts silent corruption into an honest refusal |
| 2 | **G5** — feature-matrix test | makes the rest verifiable, and stops new gaps opening |
| 3 | **G3** — check, then fix or refuse inline-data spill | small, and may already be done |
| 4 | **G2** — timestamp epoch bits and signedness | correct until 2038; do it deliberately, with tests |
| 5 | **G4** — MMP | not urgent until write support; must not be forgotten then |

Also worth doing at some point, independent of the above: correct the
two stale comments in `features.rs` (`RECOVER`, `CASEFOLD`), since a
list of caveats that overstates the gaps is only slightly better than
one that understates them.
