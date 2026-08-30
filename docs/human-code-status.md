# Human-code findings — status

Tracks every **High** and **Medium** finding from
[`human-code-report-2026-08-28.md`](human-code-report-2026-08-28.md). The report
predates the work; this is the current position. Updated 2026-08-30.

**51 findings** — 21 High, 26 Medium, 4 Low, across three groups: `X` (the
crate-wide review), `F` (the formatter) and `D` (the driver).

| | High | Medium |
|---|---|---|
| Fixed | 5 | 5 |
| Left for a human decision | 6 | 9 |
| Fixable, not yet done | 10 | 12 |

---

## Fixed here

### D2 — "Not journaled" claims that were not true — **fixed, and two of them *were* true**

The report said three such claims, none true as written. Checking each against
its body rather than taking that at face value:

- **`apply_truncate_shrink`** said *"Not journaled. Safe to call only in a
  context where crash consistency is handled elsewhere"* and promised a JBD2
  transaction as future work. The body builds a `BlockBuffer` and commits it.
  **The future work had landed and the warning outlived it**, steering callers
  away from an API that is safe.
- **`apply_replace_file_content`** said *"Not journaled — scratch-image safe"*
  and, twenty-eight lines into its own body, *"Multi-block transaction … Atomic
  across the whole replace"*. Both corrected to the second.
- **`apply_create` and `apply_mkdir`** also carry "Not journaled" — and **theirs
  is accurate**: neither builds a `BlockBuffer`. Left alone.

### F10 — two superblock offset comments contradicted the line above them — **fixed**

```rust
sb[0xE0..0xE4].copy_from_slice(&journal_inum.to_le_bytes());
// 0xDC..0xE0 s_journal_dev  — 0.
// 0xE0..0xE4 s_last_orphan  — 0.
```

The write immediately above says `0xE0` is `s_journal_inum`. This crate's own
reader agrees — it parses the journal inode from `0xE0` and documents
`s_last_orphan` at `0xE8`. Corrected, and it now says which fields are left
zero rather than mislabelling the one being written.

### F5, F6, F8, F9, F2 — **fixed earlier**

[#46](https://github.com/christhomas/rust-fs-ext4/pull/46). The three CLI bugs
(`-c` eating the device path, `-b` validated after opening the device, `-q`
order-dependent), `-F` now read, and the multi-group formatter put in front of
`e2fsck` in CI.

### F14 — the default block size written three times — **fixed**

`DEFAULT_BLOCK_SIZE` in `mkfs`, used by the CLI. Three places to change was two
chances to forget.

---

## The largest remaining items

### X1 — `Checksummer` has a `patch_` helper for every tail *except* the one written most often — High

Nine sites across driver and formatter write that tail by hand. The report's own
follow-up note says this is **larger than first stated**. Worth doing next: it
is the checksum that a wrong write corrupts silently.

### F1 — two independent formatters, one line deciding which runs — High

A structural fact about the crate that wants a decision, not a patch.

### F3 — `format_block_groups` is correct only because of a guard 360 lines away — High

The kind of coupling that survives until someone moves the guard.

### D3 — "atomic" rename is not atomic when the destination directory has to grow — **fixed**

The doc promised `replace_if_exists = true` "atomically overwrites dst", and the
body comment said the overwrite is staged "into a single buffer so a crash either
fully replaces dst or leaves the FS in its prior state". Both are true on the
common path and **false on one branch**: when the destination directory has no
room for the new entry, the buffer is committed early and the un-journaled
`extend_dir_and_add_entry` runs afterwards.

On the overwrite path the early commit has already removed dst's directory entry,
so a crash in that window leaves dst's name gone and src still present — the file
that was at dst is unreachable and nothing has moved.

The doc now names the branch, both windows, and what closing them would take:
`extend_dir_and_add_entry` staging into the buffer rather than writing on its own,
which is a change to the directory-growth path rather than to `apply_rename`. The
guarantee is stated as **atomic unless the destination directory has to grow**,
which is what the code actually does. Making it unconditionally true is a design
decision about the journal layer, not a correction.

### The second half of D3 — parent link counts read from disk mid-transaction — **fixed**

Each `i_links_count` patch read its parent inode back from disk and staged a write
of the whole record. Two patches naming the same inode in one buffer would have had
the second read the pre-buffer bytes and overwrite the first — and the only thing
preventing it was that the branch conditions happened to be mutually exclusive,
which nothing stated and nothing enforced.

Deltas are accumulated and applied once, `apply_parent_nlink_deltas`: every parent
read exactly once after every delta is known, written exactly once, and a net-zero
delta writes nothing at all. That also retires the hand-written suppression — the
dir-replaces-dir "+1 and -1 cancel" case is now arithmetic that cancels rather than
a branch suppressed by a condition that had to be kept in step with the branch it
suppressed.

Behaviour is unchanged; the four new tests pass against the old code too, which is
the point — they were missing, and the invariant was one edit away from being
false. Mutation-checked: reinstating the suppression alongside the unconditional
delta fails `a_cross_parent_directory_replace_leaves_the_destination_parent_unchanged`.

### D1 — the journal tag budget rests on a premise the same function contradicts — High

Still open.

### X1 — the dir-entry tail checksum, hand-rolled at sixteen sites — **fixed**

`Checksummer` had `patch_extent_tail` and `patch_xattr_block`. It had no
`patch_dir_entry_tail`, so the recipe written most often was the one with no
helper: plant a fake dirent (`inode = 0`, `rec_len = 12`, `name_len = 0`,
`file_type = 0xDE`), then `crc32c(seed → ino → generation → block[..len - 12])`.

**The report said nine sites. There are sixteen** — twelve in `fs.rs`, three in
`fsck.rs`, one in `mkfs.rs`, plus a seventeenth in the test suite. In two
addressing idioms (`bs - 12` with `+4/+6/+7`, and `block.len()` with
`-8/-6/-5`) a reader has to prove equivalent at each one.

#### What probing the coverage found

Before changing anything, each site's CRC span was corrupted (`end - 12` →
`end - 4`) to see which were held down by a test:

| site | tests failing, before |
|---|---|
| `seed_directory_block` (`apply_mkdir`) | **0** |
| `extend_dir_and_add_entry` | **0** |
| `extend_dir_and_add_entry_deep` | **0** |
| `extend_dir_and_add_entry_depth1` | **0** |
| `fsck::repair_wrong_dotdot` | **0** |
| `fsck` remove-entry | **0** |
| `fsck` bogus-filetype | **0** |

Not even the tests that verify dir-block checksums caught it: they check the
blocks `mkfs` wrote, not the ones the driver writes afterwards. A wrong tail
raises no error here — it surfaces when Linux mounts the volume and calls the
directory corrupt.

One of the misses is worth its own note. The first version of the growth test
created 200 files and then looked, and still saw nothing: **every later in-place
add rewrites the same block and recomputes its checksum at a different site**, so
a wrong checksum from the extension path is overwritten by a right one. The test
now stops on the create that grew the directory, so the extension path's write is
the last thing to touch the new block.

#### What is there now

`tests/dir_block_checksums_after_writes.rs` — five tests that, after mkdir,
create, unlink, rename and directory growth, walk every directory block reachable
from the root and verify its tail against `verify_dir_entry_tail`. It counts what
it checked and asserts the count, because an assertion that silently checks
nothing is the failure mode the file is about.

`Checksummer::patch_dir_entry_tail` replaces all sixteen. Planting is idempotent,
so the sites that only needed the checksum recomputed use it too rather than
keeping a second recipe. `DIR_ENTRY_TAIL_FILE_TYPE` names the `0xDE` marker.

Mutating the single helper's CRC span now fails **12 tests across three suites**.
Mutating the marker byte fails 9.

**The copy in `tests/fsck_wrong_dotdot.rs` is deliberately left alone.** A test
that computed the checksum through the helper would agree with a broken helper;
an independent restatement of the recipe is what makes it an oracle.

#### Still uncovered

The two htree variants (`extend_dir_and_add_entry_deep` / `_depth1`) and the
three `fsck.rs` repair sites are not reached by any test — but they now call the
same helper as everything else, so the recipe itself is covered even where the
call sites are not. Reaching them needs an htree directory and repair-side
fixtures.

### H1, H3, M4 — `fs.rs` at 5,258 lines; `apply_pwrite` 354 and `apply_rename` 327; 341 lines indented past column 24

All three **regressed** since the previous review, which the report notes. They
are also the whole write path, and splitting them is the largest single change
this crate could take.

### X2, X3, X4 — offsets named then bypassed on every write path; reader and writer with separate copies of the same constants; eight `#[allow]`s, six unexplained

X2 and X3 are the same problem from two sides and should be one change.

### F13 — the C ABI and the CLI disagree about oversized labels, and the library truncates mid-codepoint — Medium

Worth attention: **truncating mid-codepoint produces an invalid UTF-8 label**,
which is a defect rather than an inconsistency.

### Everything else

F4, F7, F11, F12, F15 and the remaining D-series are recorded in the report.
None are correctness claims; most are shape, naming and duplication.

---

## Verification

792 tests pass, unchanged in number. `chore lint` clean.
