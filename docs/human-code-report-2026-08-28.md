# Human-code report — 2026-08-28

> **This is analysis only. No code was changed.** Nothing in this repository was
> edited, no branch was created, and nothing was committed. The only file added by
> this session is the one you are reading. Every "shape of the fix" below is a
> proposal awaiting your confirmation.

**Scope:** the whole crate — `src/` (36 modules, 20,988 production lines) plus the
`mkfs_ext4` binary (`src/bin/mkfs_ext4.rs`, 320 lines) and the `fs_ext4_mkfs` C ABI
entry point.

**Counts:** **51 findings — 21 High, 26 Medium, 4 Low.** 0 fixed, 51 open.

**Split by body of code:** 4 cross-cutting · 15 formatter · 32 driver.

---

## Why this is split into two halves

A formatter and a reader/mutator fail in different ways, and reading them as one
body of code hides both.

The **driver** mutates a filesystem that already exists. Its failure mode is a
*partial* one: a transaction that half-lands, a checksum recomputed on three paths
out of four, a comment that describes an atomicity guarantee the code stopped
providing. Its safety net is 783 tests that drive real images and reopen them.

The **formatter** writes a filesystem from nothing. Its failure mode is *total* and
silent: every byte it lays down is unverifiable by the crate itself, because the
crate's reader shares its interpretation of the spec. Its safety net is a Linux VM
running `e2fsck` — and, as F2 shows, that net has a hole in exactly the path every
real-world volume takes.

That difference drives the "what to fix first" ordering at the end: driver items are
test-gated and can be refactored today; formatter items need tests written *before*
anything is touched.

---

## How this relates to `docs/code-quality-review-2026-08-25.md`

That review is three days old, was scoped to `src/`, and its findings still stand.
This report does not restate them. Three of its numbers should be corrected, and two
of its metrics have moved:

| Prior claim | Status now |
|---|---|
| H1 — `fs.rs` is 4,920 lines | **Now 5,258.** Still open, and growing. |
| H2 — dir-checksum tail written 3× | **Still open, and larger than stated** — see X1: 9 sites, spanning driver *and* formatter. |
| H3 — `apply_pwrite` 350 / `apply_rename` 322 lines | **Confirmed** (354 / 327). |
| M4 — 271 lines indented ≥24 columns | **Now 341.** Regressed (D29). |
| L7 — seven `#[allow(...)]` with no reason | **Now eight** (X4). |
| `fs_ext4_last_errno` is 211 lines | **Wrong.** It is 3 lines (`capi.rs:138-140`); the 211 was the gap to the next `#[no_mangle]`, filled by the `#[repr(C)]` ABI type block. Not a finding. |
| `fs_ext4_mkfs` is 223 lines | **Wrong.** It is 114 lines (`capi.rs:2390-2503`). It is still a finding, but for duplication (D8), not length. |
| `audit_inner` 293 / `plan_insert_extent_deep` 212 | **Confirmed.** |

Findings marked **[verified]** below were confirmed by running the code or by
reading the exact lines, not inferred.

---

# Part A — Cross-cutting (driver *and* formatter)

### X1 — `Checksummer` has a `patch_` helper for every tail *except* the one written most often — High

- **Category:** duplicated code / missing abstraction
- **Files:** `src/checksum.rs:205`, `src/checksum.rs:244`; write sites at
  `src/fs.rs:3833`, `src/fs.rs:4690`, `src/fs.rs:4827`, `src/fs.rs:4972`,
  `src/fsck.rs:1015`, `src/fsck.rs:1180`, `src/fsck.rs:1306`, `src/mkfs.rs:433`,
  and `src/checksum.rs:320` (its own test)
- **Coverage:** every site is exercised — `fs.rs` by the integration suite, `fsck.rs`
  by `tests/fsck_repair.rs`, `mkfs.rs` by `tests/mkfs_e2fsck_oracle.rs`. No test
  compares the nine copies to each other.

`checksum.rs` exposes `verify_dir_entry_tail` (read side) and, for its two siblings,
both halves: `patch_extent_tail` (`:205`) and `patch_xattr_block` (`:244`). There is
no `patch_dir_entry_tail`. So the write recipe — plant a fake dirent
(`inode=0`, `rec_len=12`, `name_len=0`, `file_type=0xDE`), then chain
`crc32c(seed → ino → generation → block[..len-12])` — is hand-rolled nine times.

The sharpest illustration is inside one function. At `src/fs.rs:4638` the code calls
the helper:

```rust
.patch_extent_tail(parent_ino, parent_inode.generation, &mut leaf);
```

and fifty lines later, at `src/fs.rs:4690`, the same function hand-writes the dir
tail byte by byte. Same block, same checksummer, two different disciplines.

The three `fs.rs` copies are byte-identical (diff-verified). But they are the *only*
undocumented ones: the fourth copy, in `seed_directory_block` at `src/fs.rs:3832`, is
the one that carries the explanatory comment, and it uses a different addressing
idiom (`tail = bs - 12` with `tail+4/+6/+7`, versus `end = block.len()` with
`end-8/-6/-5`). A reader must re-derive the equivalence at each site.

**Why this is the top item:** every one of those constants has to agree with
e2fsprogs, nothing forces the nine copies to agree with each other, and a divergence
produces no error until the volume is mounted somewhere else. The module already has
the exact shape needed — this is a missing sibling, not a missing design.

Related dead guard: at `:4690`, `:4827`, `:4972` the condition is
`if self.csum.enabled && reserved_tail == 12`, but `reserved_tail` was assigned
`if self.csum.enabled { 12 } else { 0 }` twenty lines earlier — the second conjunct
is always true. At the *other* csum sites (`:1634`, `:1708`, `:2251`, `:4448`,
`:4999`, `:5129`) `reserved_tail` comes from `has_csum_tail()`, where the same
spelling *is* meaningful. One expression, two meanings.

---

### X2 — the crate names its on-disk offsets, then bypasses the names on every write path — High

- **Category:** magic numbers / unnamed constants
- **Files:** constants at `src/inode.rs:21-36`; bypassed at ~55 sites in `src/fs.rs`,
  4 in `src/fsck.rs`, and throughout `src/mkfs.rs`
- **Coverage:** heavily covered behaviourally; nothing tests that the literals and the
  constants agree.

`src/inode.rs:21-36` defines sixteen `OFF_*` constants. `Inode::parse` uses them — and
then bypasses eight of them in its own body (`:135`, `:140`, `:141`, `:157`, `:162-164`,
`:213/216/219/225`). The write paths bypass them almost entirely:

| Offset | Named as | Written raw at |
|---|---|---|
| `0x04` / `0x6C` (i_size lo/hi) | `OFF_SIZE_LO` / `OFF_SIZE_HI` | `fs.rs:611`, `:616` |
| `0x1C` / `0x74` (i_blocks lo/hi) | `OFF_BLOCKS_LO` / `OFF_BLOCKS_HI` | `fs.rs:612`, `:617` |
| `0x7C` / `0x82` (i_checksum lo/hi) | `OFF_CHECKSUM_LO` / `OFF_CHECKSUM_HI` | `fs.rs` ×12, `fsck.rs:1400/1402`, `mkfs.rs:1015/1022` |
| `0x1A` (i_links_count) | `OFF_LINKS_COUNT` | `fs.rs:3852`, `fsck.rs:1047`, `:1107` |
| `0x28` (i_block) | `OFF_BLOCK` | `mkfs.rs:979` |

Three offsets have **no** constant at all and appear as bare literals in
security-relevant places: `0x14..0x18` = `i_dtime` (`fs.rs:408`, `:479`, `:2326`,
`:4350`, `:5116` — and at `:408` it doubles as the orphan-list "next" pointer,
explained only in prose); `0x02`/`0x78`/`0x18`/`0x7A` = uid/gid lo+hi
(`fs.rs:1815-1822`); `0x68`/`0x74` = `i_file_acl` lo/hi (`fs.rs:1938`, `:2095` — with
no comment at `:1938`).

**The reason this is High and not Medium:** the same literal means different fields in
different structures, in the same file.

- `0x1A` is `i_links_count` in an inode (`fs.rs:3852`) and `inode_bitmap_csum_lo` in a
  block-group descriptor (`fs.rs:1328`).
- `0x74` is `i_blocks_hi` in one place (`fs.rs:617`) and `i_file_acl_hi` in another
  (`fs.rs:1940`).
- `0x7A` (gid_hi) sits adjacent to `0x7C` (checksum_lo) and both appear as bare
  literals inside the same functions.

Two further unnamed values *disagree with each other*: the minimum usable in-inode
xattr region is `+ 4` at `fs.rs:1899`, `+ 8` at `fs.rs:1984`, and `+ 4` again at
`xattr.rs:86`. Neither value is explained.

`src/superblock.rs` — the crate's canonical parser — has **zero** named offsets across
40-plus raw hex sites (`:141`, `:149-170`, `:204-216`, `:219`, `:221`, `:228`, `:235`,
`:241`, `:244`, `:249-251`, `:258-260`).

---

### X3 — reader and writer each declare their own copy of the same on-disk constants — Medium

- **Category:** magic numbers (family not centralised)
- **Coverage:** all copies currently agree; nothing enforces it.

| Value | Reader | Writer |
|---|---|---|
| `0xEF53` | `superblock.rs:11` `EXT4_MAGIC` (pub) | `mkfs.rs:27` `EXT4_MAGIC` (private) |
| `0x0001` | `superblock.rs:18` `EXT4_VALID_FS` | `mkfs.rs:28` `EXT4_VALID_FS` |
| `0xF30A` | `extent.rs:74` `EXT4_EXT_MAGIC` | `mkfs.rs:33` `EXTENT_MAGIC` |
| inode 2 | `path.rs:26` `EXT4_ROOT_INODE` | `mkfs.rs:29` `EXT4_ROOT_INO` |
| 128 | `inode.rs:14` `INODE_BASE_SIZE` | `mkfs.rs:30` `EXT4_GOOD_OLD_INODE_SIZE`; also bare at `checksum.rs:185`, `:271`, `fs.rs:1899` |
| 32 | `inode.rs:40` `EXTRA_ISIZE_DEFAULT` | `mkfs.rs:31` `I_EXTRA_ISIZE` |
| `0x3FC` (sb csum) | `checksum.rs:85` | `mkfs.rs:212`, `:610`, `:636`, `fs.rs:1587`, `:1609` — never named anywhere |

Six values, twelve declarations, six different names. `superblock.rs` already exports
its pair publicly; `mkfs.rs` re-declares them privately rather than importing.

Also: `inode.rs:82` `EXTENTS` and `inode.rs:88` `EXTENT` are the same bit
(`0x0008_0000`) under two names in one `bitflags!` block. `EXTENT` has zero users.

---

### X4 — eight `#[allow(...)]` suppressions, six with no stated reason — Medium

- **Category:** speculative/unexplained code
- **Files:** `mkfs.rs:739`, `mkfs.rs:909`, `path.rs:134`, `fs.rs:4716`, `fs.rs:4854`
  (bare `too_many_arguments`); `block_io.rs:377` (comment explains the cleanup, not the
  suppression); `capi.rs:41` `#![allow(non_camel_case_types)]`. Two *are* justified:
  `mkfs.rs:477` and `capi.rs:365`.

Each is probably right; none says so. The two on `fs.rs:4716`/`:4854` are the lint
correctly noticing the 10-argument pair in D10.

---

# Part B — The formatter (`src/mkfs.rs`, `src/bin/mkfs_ext4.rs`, `fs_ext4_mkfs`)

**Coverage baseline for this whole section:** `src/mkfs.rs` (1,026 lines) and
`src/bin/mkfs_ext4.rs` (320 lines) contain **zero unit tests**. Every pure function in
them — `parse_size`, `parse_uuid`, `set_bitmap_range`, `build_superblock`,
`write_bgd_group`, `build_jbd2_superblock`, `group_has_super` — is reachable only
through an end-to-end format. This matters for sequencing: the skill's rule is to
prefer changes where coverage already exists, and here it does not.

### F1 — there are two independent formatters, and one line decides which runs — High

- **Category:** duplicated code / opaque naming
- **Files:** `src/mkfs.rs:68` (`format_filesystem_with_flavor`, 328 lines),
  `src/mkfs.rs:478` (`format_block_groups`, 183 lines), dispatch at `src/mkfs.rs:114`
- **Coverage:** `tests/mkfs_roundtrip.rs`, `tests/mkfs_e2fsck_oracle.rs`,
  `tests/ext2_basic.rs` — but see F2 for what is *not* covered.

```rust
if matches!(flavor, FsFlavor::Ext4) && block_size >= 2048 {
    return format_block_groups(dev, label, uuid, size_bytes, block_size);
}
```

Nothing names this rule. The only place it is explained is the error string on the
*next* branch (`"multi-group volumes require ext4 with block_size >= 2048"`). A reader
arriving at `format_block_groups` cannot tell what invariants hold on entry without
scrolling back 360 lines.

The two functions independently re-derive the same geometry:

| | single-group | multi-group |
|---|---|---|
| `RESERVED_INODES = 10` | `mkfs.rs:175` | `mkfs.rs:485` |
| `blocks_per_group = 8 * block_size` | `mkfs.rs:108` | `mkfs.rs:491` |
| `inodes_per_group = 8192` | `mkfs.rs:123` | `mkfs.rs:493` |
| `inode_table_blocks` (same `div_ceil`) | `mkfs.rs:124` | `mkfs.rs:492` |

They share only the leaf helpers (`build_root_dir`, `write_bgd_group`,
`finalize_bgd_checksum`, `build_superblock`, `write_root_inode`). The layout policy —
the part that has to agree with e2fsprogs — is written twice.

---

### F2 — the multi-group path is validated only by the reader that this crate documents as blind — High

- **Category:** comment that lies + coverage gap
- **Files:** `tests/mkfs_e2fsck_oracle.rs:14`, `tests/mkfs_e2fsck_oracle.rs:78-101`,
  `tests/mkfs_roundtrip.rs:130`; unexercised code at `src/mkfs.rs:544`,
  `src/mkfs.rs:626-640`
- **Coverage:** **this finding is the coverage gap.**

`tests/mkfs_e2fsck_oracle.rs` opens by explaining precisely why it exists:

> `mkfs_roundtrip` and `mkfs_bin_smoke` already format + re-mount through the driver's
> OWN reader, but that reader can't see a wrong checksum (the exact blind spot that hid
> the Pi corruption).

Two lines later it states a constraint that is no longer true:

> mkfs is single-block-group only, so the meaningful axis is block size

`format_block_groups` has existed since then. And working through the oracle's three
cases against the dispatch rule at `mkfs.rs:114`:

| Oracle case | Path taken | `group_count` |
|---|---|---|
| `mkfs_4k_blocks_32m` (32 MiB, 4 KiB) | `format_block_groups` | 1 |
| `mkfs_2k_blocks_16m` (16 MiB, 2 KiB) | `format_block_groups` | 1 |
| `mkfs_1k_blocks_8m` (8 MiB, 1 KiB) | single-group body | 1 |

All three produce exactly one group. So the code that only runs with two or more
groups has **never** faced `e2fsck`:

- the backup superblock + GDT loop, `src/mkfs.rs:626-640`, including the
  `s_block_group_nr` patch at `:632` and its checksum recompute at `:633`
- the short-final-group tail padding, `src/mkfs.rs:544`
- cross-group free-block accumulation, `src/mkfs.rs:539`
- the `group_has_super` sparse-super rule, `src/mkfs.rs:646`

The one test that does exercise it — `tests/mkfs_roundtrip.rs:130`, 256 MiB / 2 groups —
validates through `Filesystem::mount` plus a hand-rolled magic check on the group-1
backup. That is exactly the reader the oracle's own header says cannot see a wrong
checksum.

**This is the single most important finding in the report.** Every real-world ext4
volume this crate formats takes the multi-group path, and that path's checksum surface
has no external oracle. The fix is one test — an oracle case at, say, 256 MiB / 4 KiB —
not a refactor. It should land before any of the other formatter items, because it is
also the regression net those refactors will need.

---

### F3 — `format_block_groups` is correct only because of a guard 360 lines away — High

- **Category:** speculative/implicit coupling
- **Files:** `src/mkfs.rs:478` (no `first_data_block` computed), `src/mkfs.rs:598`
  (literal `0` passed), `src/mkfs.rs:616` (GDT hardcoded at block 1); the guard is
  `src/mkfs.rs:114`
- **Coverage:** covered for the cases that reach it; the failure mode is unreachable
  today.

The single-group path treats `first_data_block` as a first-class hazard and spends
eight lines on it (`src/mkfs.rs:213-220`):

> Block-bitmap bit `i` maps to absolute block (first_data_block + i) … For 1 KiB blocks
> first_data_block is 1, so the whole bitmap is shifted down one block relative to
> absolute numbering; indexing it by absolute block over-marks the tail by one block and
> skips the trailing pad bit (e2fsck flags both …). Work in bit space.

`format_block_groups` never computes `first_data_block` at all. It passes a literal `0`
to `build_superblock` (`:598`), writes the GDT to block 1 unconditionally (`:616`), and
does its bitmap arithmetic relative to `gstart`. All of that is correct — but only
because `block_size >= 2048` at the call site forces `first_data_block == 0`.

Nothing in the function says so. Whoever relaxes that dispatch guard to let 1 KiB
volumes span groups reintroduces, silently, the exact bug the other path documented at
length.

---

### F4 — `build_superblock` takes 14 positional arguments, and one call site passes the same variable into two of them — High

- **Category:** too many parameters
- **Files:** `src/mkfs.rs:740` (signature), `src/mkfs.rs:186-200` and
  `src/mkfs.rs:593-607` (call sites)
- **Coverage:** exercised end-to-end; a transposition would be caught only by `e2fsck`,
  and only on the paths F2 shows are checked.

```rust
fn build_superblock(
    inodes_count: u32, blocks_count: u64, free_blocks: u64, free_inodes: u32,
    first_data_block: u32, log_block_size: u32, blocks_per_group: u32,
    inodes_per_group: u32, uuid: &[u8; 16], label: &str, flavor: FsFlavor,
    inode_size: u16, desc_size: u16, journal_inum: u32,
) -> Vec<u8>
```

Eight leading numeric arguments, mostly `u32`/`u64`. Any transposition among them
compiles and produces a superblock that is wrong in a way no Rust-side test can see.

It gets worse at the single-group call site (`src/mkfs.rs:186`), which passes
`inodes_per_group` into **both** position 1 (`inodes_count`) and position 8
(`inodes_per_group`) — correct, because a single-group volume has exactly one group's
worth of inodes, but the reader has to work that out. The multi-group site passes
`inodes_count as u32` into position 1. The same parameter means two different
quantities depending on which formatter called.

`write_bgd_group` (`src/mkfs.rs:910`) has the same shape at 8 parameters, including
three adjacent unlabelled counters (`free_blocks: u64, free_inodes: u32, used_dirs: u32`).

---

### F5 — the CLI treats `-c` as taking an argument; it does not, and it eats the device path — High **[verified]**

- **Category:** magic values (unnamed flag list) / comment that lies
- **Files:** `src/bin/mkfs_ext4.rs:228` (the parser), `src/bin/mkfs_ext4.rs:57` (the help)
- **Coverage:** none. `tests/mkfs_bin_smoke.rs` covers four cases (basic format,
  `--create-size` create, `--create-size` idempotent, dry-run) and no flag-parsing case.

```rust
"-m" | "-N" | "-i" | "-c" | "-E" | "-O" | "-T" => {
    let v = args.next().ok_or_else(|| format!("{arg} requires an argument"))?;
```

In the conventional CLI `-c` is a boolean (check for bad blocks). Here it consumes the
next argument. Run against the built binary:

```console
$ mkfs_ext4 -c /tmp/hc_t.img
mkfs.ext4: warning: -c /tmp/hc_t.img not yet honored, ignoring
mkfs.ext4: missing positional <device> argument
```

The device path is swallowed as `-c`'s value and the tool then complains it was never
given one.

The help text disagrees with the parser about which flags are even in the list
(`src/bin/mkfs_ext4.rs:57`):

> Unsupported flags from the standard CLI are accepted with a warning if they take an
> argument we can ignore safely (**-m, -N, -i**)

Three named in the docs, seven in the code. The list is a bare `|` chain in a match
arm with nothing tying it to the documentation or to the upstream flag semantics.

---

### F6 — `-b` is validated 90 lines and one `open(O_RDWR)` too late — Medium **[verified]**

- **Category:** duplicated/divergent validation
- **Files:** `src/bin/mkfs_ext4.rs:207` (parses, no check), `src/mkfs.rs:97` (checks)
- **Coverage:** none.

```console
$ mkfs_ext4 -b 3000 /tmp/hc_t.img
mkfs.ext4: formatting /tmp/hc_t.img (33554432 bytes, block_size=3000)
mkfs.ext4: format failed: InvalidArgument("mkfs: block_size out of range")
```

The rule (power of two, `1024..=65536`) is printed in the help at
`src/bin/mkfs_ext4.rs:31`, enforced in `src/mkfs.rs:97`, and absent from the flag that
needs it — so the device is opened read-write and a "formatting" line is printed before
the tool discovers the argument was never valid.

---

### F7 — `run()` is 109 lines doing four jobs — Medium

- **Category:** god function / deep nesting
- **Files:** `src/bin/mkfs_ext4.rs:88`
- **Coverage:** `tests/mkfs_bin_smoke.rs` (4 cases, all happy-path).

Argument parsing (`:89`), `--create-size` file provisioning (`:103-152`), device open
and size validation (`:155-165`), then format + flush + report (`:167-192`). The middle
block is 50 lines and nests four deep through a `#[cfg(unix)]` inside a `match` arm
inside an `if let`, to answer one question: *does this path need creating, and is it
safe to create?* It is a `fn provision_image(device: &str, size: u64, quiet: bool)`
that has not been extracted yet.

---

### F8 — `-F` is parsed, documented, and never read — Medium

- **Category:** speculative code
- **Files:** `src/bin/mkfs_ext4.rs:177`, `src/bin/mkfs_ext4.rs:198`
- **Coverage:** none.

```rust
let _ = opts.force; // suppress unused warning when neither path uses it
```

The comment states the situation exactly: neither path uses it. The statement sits
inside the *dry-run* branch, where it reads as though `force` were somehow relevant to
dry runs. The help at `:35` already tells the truth ("Accepted; we do not currently
inspect for active mounts") — the `let _ =` adds nothing the reader needs and one thing
they must discount.

Same file, same category: `src/bin/mkfs_ext4.rs:198` builds
`std::env::args().skip(1).peekable()` and never calls `.peek()`.

---

### F9 — `-q` only silences flags that appear after it — Medium **[verified]**

- **Category:** dense/order-dependent logic
- **Files:** `src/bin/mkfs_ext4.rs:198-260`
- **Coverage:** none.

```console
$ mkfs_ext4 -n -q -m 1 img     # silent
$ mkfs_ext4 -n -m 1 -q img
mkfs.ext4: warning: -m 1 not yet honored, ignoring
```

`opts.quiet` is read *during* the parse loop (`:236`), so whether a warning prints
depends on flag order. Separating "collect options" from "act on options" removes the
ordering dependency entirely.

---

### F10 — two superblock offset comments in `mkfs.rs` contradict the line above them and the crate's own reader — Medium **[verified]**

- **Category:** comment that lies
- **Files:** `src/mkfs.rs:837-841`; contradicted by `src/superblock.rs:81` and
  `src/superblock.rs:260`
- **Coverage:** the code is correct and covered; only the comments are wrong.

```rust
sb[0xE0..0xE4].copy_from_slice(&journal_inum.to_le_bytes());
// 0xDC..0xE0 s_journal_dev  — 0.
// 0xE0..0xE4 s_last_orphan  — 0.
```

The second comment claims `0xE0` is `s_last_orphan`, four bytes after the code just
wrote `s_journal_inum` there. The real layout — and the crate's own reader agrees — is
`s_journal_inum` at `0xE0`, `s_journal_dev` at `0xE4`, `s_last_orphan` at `0xE8`
(`superblock.rs:81`, `:260`). Both comments are off by four, inside the one function
where a reader is relying on the comments to check the offsets by eye.

---

### F11 — rustfmt has pushed 14 offset-documenting comments out to column 55-75 — Medium **[verified]**

- **Category:** comments that obstruct
- **Files:** `src/mkfs.rs:667-675`, `:928-946`, `:984-985`, `:1008-1010`
- **Coverage:** n/a. `cargo fmt --check` passes, so this is the committed shape.

Each block starts with a trailing comment, and rustfmt aligns every following
standalone comment to that column:

```rust
    buf[0x08..0x0C].copy_from_slice(&1u32.to_be_bytes()); // h_sequence
                                                          // s_blocksize, s_maxlen, s_first, s_sequence, s_start, s_errno.
```

In `write_bgd_group`, `write_journal_inode`, `build_jbd2_superblock` and
`write_root_inode` — the four functions whose entire content is offsets — the
documentation of those offsets is the hardest text in the file to read. Moving each
comment onto its own line *above* the statement it describes restores natural
indentation, and rustfmt leaves it there.

---

### F12 — the ext2/ext3 branches are unreachable from every shipping entry point — Medium

- **Category:** speculative code / god function
- **Files:** `src/mkfs.rs:68`; entry points at `src/bin/mkfs_ext4.rs:181` and
  `src/capi.rs:2492`
- **Coverage:** `tests/ext2_basic.rs`, `tests/verify_basic.rs`, `tests/mkfs_ext3_oracle.rs`
  (2 cases `#[ignore]`d — the ext3 journal is not `e2fsck`-clean).

Both shipping callers invoke `format_filesystem`, which pins `FsFlavor::Ext4`.
`format_filesystem_with_flavor` is reachable only from Rust tests. The flavor branching
threaded through it — `ext3_journal_blocks`, `journal_indirect_blocks`, `journal_end`,
the whole journal-inode section at `:295-332`, the per-flavor feature match at `:803` —
is the main reason the function is 328 lines, and none of it can be reached from the CLI
or the C ABI.

This is not an argument for deleting it. It is an argument that the ext3 path is a
*separate* formatter wearing the same function as the ext4 one, and splitting it out
would take the flagship function's length down by more than any other single change.

---

### F13 — the C ABI and the CLI disagree about oversized labels, and the library truncates mid-codepoint — Medium

- **Category:** divergent duplicated validation
- **Files:** `src/bin/mkfs_ext4.rs:211-222` (rejects), `src/capi.rs:2474-2478` (passes
  through), `src/mkfs.rs:829-831` (truncates)
- **Coverage:** `tests/mkfs_bin_smoke.rs` uses only short ASCII labels.

```rust
let n = lbl.len().min(16);
sb[0x78..0x78 + n].copy_from_slice(&lbl[..n]);
```

That is a **byte** slice. A 17-byte UTF-8 label arriving through `fs_ext4_mkfs` is cut
at byte 16, which can land mid-codepoint and write a broken sequence into
`s_volume_name`. The CLI never hits this because it rejects `> 16` first — so the
stricter of the two callers is the one that made the library's truncation look safe.
The C ABI also collapses "null label" and "non-UTF-8 label" to the same `None`.

---

### F14 — the default block size `4096` is written three times with no shared constant — Medium

- **Category:** magic numbers
- **Files:** `src/capi.rs:2418`, `src/bin/mkfs_ext4.rs:95`, `src/bin/mkfs_ext4.rs:33`
  (as prose in the help text)
- **Coverage:** all three exercised; nothing checks they agree.

`src/mkfs.rs`'s const block (`:27-33`) is the obvious home and has no
`DEFAULT_BLOCK_SIZE`.

---

### F15 — `set_bitmap_range` clamps by silently breaking — Low

- **Category:** speculative/defensive code
- **Files:** `src/mkfs.rs:394-403`
- **Coverage:** exercised by every format.

```rust
for bit in start..end {
    let byte = (bit / 8) as usize;
    if byte >= bitmap.len() { break; }
```

A caller that computes an over-long range gets exactly the same behaviour as one that
computes it correctly. Given that both formatters call this with arithmetic derived
from `first_data_block` (F3), a range that is silently truncated is the failure this
helper is most likely to be asked to absorb.

---

# Part C — The driver

### D1 — the journal tag budget rests on a premise the same function contradicts — High

- **Category:** invariant stated only in a comment
- **Files:** `src/fs.rs:3121-3127`, contradicted at `src/fs.rs:3222`
- **Coverage:** `tests/capi_pwrite.rs`, `tests/journal_writer_crash_pwrite_rmdir_xattr.rs`
  cover pwrite; none targets a fragmented multi-group chunk.

`apply_pwrite` sizes its chunks so a transaction's descriptor block cannot overflow,
reserving 8 tag slots for metadata. The comment justifying `8` says a chunk
"always lands in a single block group … so the real overhead is <= 4".

Ninety-five lines later, in the same function, the fragmentation fallback at
`src/fs.rs:3222` halves `want` and loops, calling `plan_block_allocation` afresh each
time (`:3214`) and staging its own bitmap + BGD blocks — while `alloc_closure`
(`:3272-3283`) adds unbounded extent-tree metadata blocks on top. A sufficiently
fragmented chunk touches many groups and blows well past 8 metadata tags.

Nothing checks the tag budget at commit time. The premise is load-bearing, stated once,
in prose, and falsified in the same scroll.

---

### D2 — three "Not journaled" doc claims, none of them true as written — High

- **Category:** comments that lie
- **Files:** `src/fs.rs:640-643`, `src/fs.rs:2794` vs `src/fs.rs:2822-2825`,
  `src/fs.rs:2812-2818`
- **Coverage:** `tests/fallocate_crash_safety.rs`, `tests/all_images_rw_smoke.rs` and the
  journal_writer suite cover the behaviour; nothing checks the docs.

1. `apply_truncate_shrink` (`:640`) warns callers it is "**Not journaled.** Safe to call
   only in a context where crash consistency is handled elsewhere (e.g. a test scratch
   image)" and promises "A future revision will route this through a JBD2 transaction".
   The body at `:668-698` is fully journaled — `BlockBuffer::new` →
   `buffer_free_block_run_and_bgd` → `buffer_write_inode` → `commit_block_buffer`. The
   future revision landed; the doc still steers callers away from a safe API.

2. `apply_replace_file_content` says "**Not journaled** — scratch-image safe" at `:2794`
   and, 28 lines later, "Multi-block transaction … **Atomic across the whole replace**"
   at `:2822`. Two statements about one function, directly contradictory.

3. The doc is accidentally true for the *other* delegate. `:2812-2818` says the ext2/3
   path has the "Same overall shape as the extent path below", but
   `apply_replace_file_content_indirect` (`:2926`) uses direct `free_block_run_and_bgd` /
   `patch_sb_counters` / `dev.write_at` / `dev.flush` and really is non-atomic. One
   public entry point, two crash semantics, and the comment says they are the same.

---

### D3 — "atomic" rename is not atomic on the overwrite path — High

- **Category:** comment that lies about a safety property
- **Files:** `src/fs.rs:4093` (doc), `src/fs.rs:4208-4210` (comment) vs
  `src/fs.rs:4237-4248`; repeated at `src/fs.rs:4372-4374` vs `:4390-4399`
- **Coverage:** `tests/capi_rename.rs`, `tests/capi_rename_overwrite.rs` cover success
  paths; no crash-injection test on the extend branch.

The doc promises `replace_if_exists = true` "atomically overwrites dst", and the body
comment says the overwrite is staged "into a single buffer so a crash either fully
replaces dst or leaves the FS in its prior state". Then `:4240` commits that buffer —
dst's directory entry is now gone — and calls the un-journaled
`extend_dir_and_add_entry`. If that fails, dst's name has been removed and src still
exists. The same pattern appears on the no-overwrite path at `:4372` vs the
two-transaction `dst_extends` split at `:4390`.

Related, in the same function: nlink patching reads inodes from **disk**
(`read_inode_verified` at `:4262`, `:4271`, `:4353`, `:4412`, `:4418`) while a write to
the same inode may already be staged in `buf`. It is correct today only because the
branch conditions at `:4258` and `:4353` are mutually exclusive — an invariant nothing
states and nothing enforces.

---

### D4 — 106 lines of dead, weaker duplicates, hidden by a crate-wide `allow(dead_code)` — High **[verified]**

- **Category:** speculative code / duplication
- **Files:** `src/fs.rs:4430` `add_dir_entry` (55 lines), `src/fs.rs:4988`
  `remove_dir_entry` (31), `src/fs.rs:5025` `update_dotdot` (20); masked by
  `src/lib.rs:18`
- **Coverage:** none — that is the point.

Zero callers across `src/`, `tests/` and `examples/` (verified). Each is a stale
near-duplicate of a live `buffer_*` twin, and each is *behaviourally weaker*: they call
`crate::extent::map_logical` directly (`:4443`, `:4994`, `:5027`) instead of
`self.map_inode_logical` (`:560`), so they silently fail on non-extent (ext2/ext3)
inodes that the live twins handle.

A maintainer chasing a checksum bug can plausibly land the fix in the copy nothing
calls. `#![allow(dead_code)]` at `src/lib.rs:18` is what keeps the compiler from saying so.

Same category, elsewhere: `src/block_io.rs:198-319` `CachingDevice` is a complete second
LRU block cache with **zero users** (verified — `block_cache::CachedDevice` is what
`fs.rs:301` instantiates), and it declares a private `struct CacheState` (`:220`) that
collides by name with `block_cache.rs:52`. Roughly 200 lines plus 95 lines of its own
tests.

---

### D5 — `verify_inode` fails open where every sibling fails closed — High **[verified]**

- **Category:** dense logic / inconsistent policy
- **Files:** `src/checksum.rs:187`
- **Coverage:** `src/checksum.rs` unit tests cover the enabled/disabled axis, not the
  short-buffer axis.

```rust
pub fn verify_inode(&self, ino: u32, generation: u32, inode_raw: &[u8]) -> bool {
    if !self.enabled || inode_raw.len() < 128 {
        return true;          // <-- passes
```

versus `verify_superblock` (`:82`), `verify_dir_entry_tail` (`:139`) and
`verify_extent_tail` (`:171`), which all split the two conditions and `return false` on
a short buffer. A truncated inode read silently verifies. The difference is one `||`
against two separate `if`s, and it is invisible unless you read all four.

---

### D6 — a non-UTF-8 or oversized path is applied to `/` on six mutating entry points — High

- **Category:** comment that lies (about a safety contract)
- **Files:** `src/capi.rs:346` (the false claim), `src/path.rs:288` + `src/path.rs:74-86`
  (the actual behaviour), `src/path.rs:336-340` (a test asserting it)
- **Coverage:** `tests/capi_resilience.rs` covers null and bad handles, not invalid UTF-8.

`cstr_to_str` documents its contract as: *"downstream path resolution treats `""` as
ENOENT."* It does not. `split_path` (`path.rs:288`) filters empty components, so
`lookup_with_csum` returns the root inode for `""` — and `path.rs:336` asserts exactly
that (`for root_path in ["/", "", "///"]`). `cstr_to_str` returns `""` for oversize
(> 4096) and invalid-UTF-8 input.

So such a path reaching `fs_ext4_chmod` (`:2038`), `fs_ext4_chown` (`:2070`),
`fs_ext4_utimens` (`:2178`), `fs_ext4_set_flags` (`:2144`), `fs_ext4_setxattr`
(`:2326`) or `fs_ext4_removexattr` (`:2288`) is applied to the root directory and
returns 0.

`fs_ext4_symlink` (`:2236-2262`) noticed — its comment says *"The generic
`cstr_to_str` silently returns `""` past the cap"* — and hand-rolled a length check
that fixes the oversize case at one call site, leaving the UTF-8 case and every other
entry point alone. The strict helper that would fix it properly,
`cstr_to_str_strict` (`:366`), is `#[allow(dead_code)]` and unused.

---

### D7 — the fsck section header says "never writes to disk"; repair ships in that entry point — High

- **Category:** comment that lies
- **Files:** `src/capi.rs:2506-2513` (section header), `src/capi.rs:2708` (fn doc);
  contradicted by `src/capi.rs:2586-2589`, `:2743-2751`, `:2823-2830`
- **Coverage:** `tests/fsck_repair.rs` (841 lines) covers the repair path thoroughly.

> `// Architecture note: this is a *read-only* fsck — it never writes to disk.`
> `// Repair … is explicit future work and will require a journaled write path plus a
> new ABI entry point.`

`fs_ext4_fsck_options_t.repair` exists at `:2586`, is validated at `:2743`, and is
dispatched to `fsck::audit_with_repair` at `:2823`, which commits through the journal.
No new entry point was needed. A reader auditing the crate's write surface from these
comments would exclude the function.

---

### D8 — the callback block device is constructed three times, and the copies have already drifted — High

- **Category:** duplicated code
- **Files:** `src/capi.rs:604-699`, `src/capi.rs:711-801`, `src/capi.rs:2423-2472`
  (inside `fs_ext4_mkfs`)
- **Coverage:** `tests/capi_callback_rw.rs`, `tests/capi_lazy_replay.rs`.

`mount_rw_with_callbacks_inner` and `mount_rw_with_callbacks_lazy_inner` are 87
identical lines out of 91, differing only in `Filesystem::mount` vs `mount_lazy` and
one error-context string. `fs_ext4_mkfs` carries a third copy — and the drift is
already visible:

```rust
// capi.rs:634  (mount)
Err(std::io::Error::other(format!("read callback returned {rc}")))
// capi.rs:2434 (mkfs)
Err(std::io::Error::other(format!("read cb {rc}")))
```

Same for write (`:652` vs `:2449`) and flush (`:669` vs `:2459`). Because the mkfs copy
is inlined inside the `ffi_guard` closure instead of extracted, it sits at six indent
levels (`:2445-2461`) where the identical code in the helpers sits at two or three.

For the record: `fs_ext4_mkfs` does **not** duplicate `format_filesystem` — it
delegates correctly at `:2492`, exactly as the CLI does at `bin/mkfs_ext4.rs:181`. The
duplication is entirely in the device bridge.

---

### D9 — the htree reader and writer disagree by four bytes about where entries start — High **[verified]**

- **Category:** duplicated logic that diverged
- **Files:** `src/htree.rs:131` and `:173` vs `src/htree_mut.rs:240`
- **Coverage:** each module's own unit tests bless its own layout. `htree_mut` has no
  in-crate caller — `src/lib.rs:44` (`pub mod htree_mut;`) is its only reference outside
  its own tests (verified).

`htree.rs` uses the kernel model: `dx_entry[0]` **overlays** the `count/limit` word at
`cl_offset`, and the reader zeroes its `hash` afterwards because the bytes it just
parsed as a hash are really `(limit, count)`.

`htree_mut.rs:240` starts the entry array four bytes later:

```rust
let entries_start = cl_offset + 4;
```

and skips slot 0 entirely. Nothing exercises the writer against the reader, which is
why a four-byte layout disagreement about an on-disk index has never surfaced.

`htree_mut.rs` also contains a fourth hand-rolled dirent walk (`:68-117`, duplicating
`dir.rs:276-304` and `:351-364`), reimplements `dir::entry_rec_len` at `:106`
(`8 + ((name_len + 3) & !3)`), and writes `rec_len` directly at `:142`.

---

### D10 — two ten-argument functions with identical parameter lists, carrying a redundant pair that can desynchronise — High

- **Category:** too many parameters
- **Files:** `src/fs.rs:4717` `extend_dir_and_add_entry_deep`, `src/fs.rs:4855`
  `extend_dir_and_add_entry_depth1`
- **Coverage:** `tests/extent_deep_insert.rs`, `tests/extent_multi_level.rs`.

Both take the same 10 arguments in the same order, differing only in the last one's
name (`data_plan` vs `plan`). Both carry `new_phys` **and** the plan, where
`new_phys == plan.first_block` was established at `:4548` — two values that must agree
and nothing keeps them agreeing. `parent_ino` + `parent_inode` + `parent_raw` travel
together everywhere and are a struct in disguise.

`_depth1` then recurses into `_deep` (`:4903-4914`) passing all ten through unchanged;
the only thing asserting the plan is still uncommitted at that point is the comment at
`:4899`.

---

### D11 — seven implementations of "stamp an inode checksum", two of them with swapped argument order — High

- **Category:** duplicated code / misleading names
- **Files:** `src/fs.rs:2009` `finalize_inode_raw(ino, generation, raw)`,
  `src/fs.rs:2517` `stamp_inode_checksum(raw, ino, generation)`; inlined at
  `src/fs.rs:3462`, `:3854`, `:4655`, `:4795`, `:4938`
- **Coverage:** widely exercised; nothing compares the seven.

Byte-identical behaviour, **opposite parameter order**, one using raw hex offsets and
one using the named constants. Both take two adjacent `u32`s, so transposing
`(ino, generation)` compiles and yields a wrong-but-plausible checksum — the exact
class of bug the crate's own history (the metadata_csum series) says takes a real
`e2fsck` to catch.

---

### D12 — the extent header write is copy-pasted six times, with `max` computed three different ways — High

- **Category:** duplicated code that diverged
- **Files:** `src/extent_mut.rs:92`, `:224`, `:242`, `:428`, `:452`, `:469`
- **Coverage:** `tests/extent_deep_insert.rs`, `tests/extent_multi_level.rs`.

No `write_extent_header()` helper exists. The five-field header is written inline six
times, and the copies already differ on the `max` field: `header.max` at `:96`,
`node_max_entries()`-derived at `:228`/`:432`/`:456`, hardcoded `4u16` at `:246`/`:473`.
Lines `:246` and `:473` are byte-identical 116-character lines, trailing comment
included.

Nearby, `build_depth1_index_root` (`:251-253`) hand-writes an index entry that
`encode_index` (`:376-384`) already encodes, using raw `12/16/20/22` offsets where
`extent_entry_offset(0)` (`:41`) yields the base.

---

### D13 — eleven levels of nesting, and a closure written twice within one function — High

- **Category:** deep nesting / duplication
- **Files:** `src/fs.rs:3290-3324` (peak at `:3295`, 44-column indent);
  `src/fs.rs:3206-3213` and `:3293-3298`
- **Coverage:** `tests/capi_pwrite.rs`.

The nest is fn → `while lb` → `while remaining_in_run` → `match` → `Err` arm →
block-expr → `alloc_closure` → block-expr → `bitmap_reader` → `if let Some`. Three of
those levels exist only to scope borrows.

Inside it, the `buf.dirty`-aware bitmap-reader closure appears twice, verbatim, 85 lines
apart, in the same function. Both copies reach directly into `BlockBuffer.dirty` — a
`BTreeMap` field — bypassing `get_mut`/`put`. `apply_pwrite` is the only code outside
`BlockBuffer`'s own impl (`:49-60`) and `commit_block_buffer` (`:1759-1788`) that does
this.

---

### D14 — fsck's repair writers bypass the offset constants the read path uses — High

- **Category:** magic numbers
- **Files:** `src/fsck.rs:1047`, `:1107` (`0x1A..0x1C`), `:1400` (`0x7C..0x7E`),
  `:1401` (`len() >= 0x84`), `:1402` (`0x82..0x84`)
- **Coverage:** `tests/fsck_repair.rs`.

`inode.rs:26`/`:33`/`:35` define `OFF_LINKS_COUNT`, `OFF_CHECKSUM_LO`,
`OFF_CHECKSUM_HI`, and `Inode::parse` uses them (`inode.rs:143`, `:166`). The repair
path — which *writes* those same fields, and is the code most likely to be wrong — uses
bare literals. Read path and repair path can drift with no compiler help. Compounded by
X2: `0x1A` also means `inode_bitmap_csum_lo` in a block-group descriptor two files away.

The three fsck dir-block CRC recomputes (`:1012-1019`, `:1178-1184`, `:1304-1310`) and
their three `reserved_tail = if … { 12 } else { 0 }` companions (`:1006`, `:1146`,
`:1270`) are the fsck-side instances of X1; the comment at `:1176` acknowledges the
copy (*"Same recipe as repair_duplicate_dir_inode — see comments there"*).

---

### D15 — two opposite commit policies for the same allocation, each documented as correct where it stands — Medium **[verified]**

- **Category:** contradictory rationale
- **Files:** `src/fs.rs:4609-4611` (eager) vs `src/fs.rs:4734-4739` (late)
- **Coverage:** `tests/extent_deep_insert.rs`.

At `:4611` the depth-0 promotion path commits the data-block allocation immediately,
with a comment justifying it: *"Commit the data-block allocation NOW so the next plan
picks a different run (plan_block_allocation reads the bitmap)."*

At `:4734` the deep path explains the opposite policy, and why: *"Committing eagerly
(old behaviour) leaked blocks permanently when plan_insert_extent_deep or the subsequent
writes failed … we gather all plans and commit them only after every write succeeds,
matching the late-commit ordering of `extend_dir_and_add_entry_depth1`."*

Two of the three paths use late commit and name each other; the third uses eager commit
and names neither. Both comments are locally persuasive. Neither acknowledges the other,
so a reader cannot tell which is the intended rule of the module — and the deep path's
comment describes a leak that, by its own reasoning, the depth-0 path is still exposed
to on failure between `:4628` and `:4666`.

---

### D16 — four different bitmap readers feed the same planner — Medium

- **Category:** duplicated code with divergent semantics
- **Files:** `src/fs.rs:4540` (`self.read_block`), `:4614` and `:4750` (raw
  `dev.read_at`, bypassing `read_block`), `:3208` (`buf.dirty`-aware closure)
- **Coverage:** exercised; no test distinguishes them.

Only the last sees uncommitted bitmap state. Nothing documents which is correct where.

---

### D17 — the three `extend_dir_and_add_entry*` functions are ~65% verbatim, and only one verifies before mutating — Medium

- **Category:** duplicated code
- **Files:** `src/fs.rs:4520` (197 lines), `:4717` (138), `:4855` (133)
- **Coverage:** `tests/extent_deep_insert.rs`, `tests/extent_multi_level.rs`.

Genuinely different: only the extent-insert step (inline root / single index / deep
planner). Shared verbatim: the inode-checksum stamp (`:4655` / `:4795` / `:4938`), the
block seed + `add_entry_to_block` + csum tail + `write_at` sequence (`:4670` / `:4809` /
`:4952` — this is X1), and the `new_size`/`new_blocks` arithmetic differing only in a
multiplier. Deduplicating the first two removes ~100 of the 468 lines.

Worth noting while reading them: `_depth1` is the **only** variant that CRC-verifies the
leaf before mutating it (`:4886-4894`).

---

### D18 — 39 ABI entry points, ~470 lines of identical boilerplate, no macro — Medium

- **Category:** duplicated code
- **Files:** `src/capi.rs` — 41 `#[no_mangle]` fns, 39 `ffi_guard` sites, 36
  `clear_last_error()`; canonical shape at `src/capi.rs:2043-2061`
- **Coverage:** `tests/capi_*.rs` (16 files).

Roughly 16% of the file. The good news, and it is worth recording: **the divergences
were audited and no entry point is missing a null check or a `catch_unwind`.** Two
skip `clear_last_error()` benignly (`fs_ext4_umount:827`, `fs_ext4_dir_close:1154`).

The one that matters is `fs_ext4_dir_next` (`src/capi.rs:1124-1150`): it neither clears
nor sets the thread-local error, and returns `null()` for **both** "null iterator"
(`:1131`) and "end of iteration" (`:1135`). The module contract at `:38-39` promises
`last_error` is valid until the next FFI call, so a C caller writing the obvious
`while ((d = fs_ext4_dir_next(it))) …; if (fs_ext4_last_errno()) …` reads a stale errno
from an unrelated call and cannot distinguish clean EOF from a bad handle.

---

### D19 — the allocator and the commit path model an uninitialised group differently — Medium

- **Category:** duplicated logic that diverged
- **Files:** `src/alloc.rs:237-241` vs `src/fs.rs:1263-1281` +
  `group_owned_metadata_blocks` (`src/fs.rs:1189`)
- **Coverage:** `tests/uninit_group_flags_cleared.rs` covers the commit side (PR #44).

`alloc.rs:237` substitutes an all-zero bitmap for a `BLOCK_UNINIT` group — the whole
group free, no metadata reserved. `fs.rs:1263` correctly reserves the group's own
superblock backup, GDT, bitmaps and inode table. PR #44 fixed the commit side; the
planner still models it the old way, so it can propose the group's own metadata blocks.

Same module: `alloc.rs` has **no reserved-inode floor** at all (compare
`verify.rs:302`, which uses `sb.first_inode`), and its own test at `alloc.rs:610`
asserts `plan.inode == 1, "first inode in group 0 is ino 1"` — blessing the handing out
of a reserved inode on an `INODE_UNINIT` group.

---

### D20 — a CSUM_V2 predicate is passed into a `uses_csum_v3` parameter — Medium

- **Category:** misleading name / diverged constant
- **Files:** `src/journal_writer.rs:127` → `src/transaction.rs:70`; reader at
  `src/journal.rs:235`
- **Coverage:** `tests/journal_writer_*.rs` (several); all fixtures use v3.

The writer passes `jsb.uses_csum_v2_or_v3()` into a parameter named `uses_csum_v3`,
while the reader keys on `CSUM_V3.bits()` alone. On a CSUM_V2-only journal the writer
emits v3-shaped tags and the reader parses classical ones.

The `tag_size` ternary itself is duplicated byte-for-byte at `transaction.rs:124-134`
and `journal.rs:239-249` — they must stay in lockstep or replay breaks, and nothing
enforces that.

---

### D21–D29 — the remaining Medium items

| ID | Finding | Category | Files | Coverage |
|---|---|---|---|---|
| D21 | `CachingDevice` is a dead second LRU cache (~200 lines + 95 test lines) whose private `CacheState` collides by name with `block_cache.rs:52` | speculative code | `src/block_io.rs:198-319`, `:445-540` | its own tests only |
| D22 | `superblock.rs` has zero named offsets across 40+ hex sites; `inode.rs` defines 16 `OFF_*` then bypasses 8 in its own parser | magic numbers | `src/superblock.rs:141-260`, `src/inode.rs:135-225` | heavily exercised |
| D23 | Stale module headers: `checksum.rs:22` "Phase 1 … no writes yet" (it has three patch fns); `alloc.rs:3-7` "E11 will apply the plans" (E11 shipped, 16 call sites); `capi.rs:5-30` lists 23 of 41 entry points and is still organised as "Phase 1 / Phase 4 (in progress)"; plus stale "future"/"follow-up" claims at `fs.rs:2189`, `:2405`, `:4428` | comments that lie | as listed | n/a |
| D24 | A dangling doc comment attaches to the wrong function — the text at `fs.rs:4486-4490` describes `extend_dir_and_add_entry` but sits above `commit_dir_block_alloc` (`:4498`), so rustdoc concatenates both; its claim ("assumes the inline root still has a free slot") has been false since `:4565`/`:4579` | comment that lies | `src/fs.rs:4486-4497` | n/a |
| D25 | `apply_symlink`'s public doc is wrong three ways: `<=` vs `<` 60 (`:2607`, `:2674`), the "61..=255" slow range (actual: `60..=min(4096, block_size)`), and a `NameTooLong` the code never returns (`:2586`) | comment that lies | `src/fs.rs:2568-2575` | `tests/capi_long_symlink.rs` |
| D26 | `plan_insert_extent_deep`: three near-identical split-and-emit blocks (`:725`, `:797`, `:824`); sorted-insert written three times (`:162`, `:287`, `:512`); `propagated` declared uninitialised then assigned only in an `else` after a block whose every arm returns (`:679`, `:701`) — which is what makes 212 lines read as one flat body; `.expect(...)` at `:750` turns a planner logic bug into EIO through `ffi_guard` | god function / duplication | `src/extent_mut.rs:637-848` | `tests/extent_deep_insert.rs` |
| D27 | fsck progress throttling is inconsistent with the 1 Hz rule: the Inodes phase throttles at an unnamed 500 ms (`:447`, i.e. 2 Hz); the Directory phase does not throttle at all — `emit_dir_progress` (`:691`) is called once per directory from `:358`, `:376`, `:385`, `:393`, `:431` | magic number / divergence | `src/fsck.rs:446-448`, `:691-699` | `tests/fsck_repair.rs` |
| D28 | fsck repair hand-decodes dirent records twice with raw offsets (`off+4/+5/+6/+7/+8`) although `DirBlockIter` already parses that layout and is used at `:709`, `:730`; the comment at `:1153` admits it | duplication | `src/fsck.rs:1157-1171`, `:1278-1301` | `tests/fsck_repair.rs` |
| D29 | 341 lines indented ≥24 columns — up from 271 three days ago; 209 in `fs.rs`, 29 in `capi.rs`, 20 in `verify.rs` | deep nesting | crate-wide | n/a |

Also folded into this tier rather than listed separately: `fsck.rs:1329`/`:1368` take 7
and 6 parameters, four of which exist only to be subtracted immediately (`:1338`,
`:1376`), and are asymmetric in their range-checking (`:1344` checks both deltas,
`:1381` checks one) with no explanation; `fsck.rs:1355` passes a bare `0` as the fourth
numeric argument to `patch_bgd_counters` (it is the dirs delta); `capi.rs:1104-1113`
writes dirent `file_type` as bare literals that must match `fs_ext4_file_type_t`
(`:149`), `mode_to_file_type` (`:378`) and `dir::DirEntryType` — four parallel encodings
of one 0-7 space; `capi.rs:867`/`:876`/`:1098` hand-compute `.min(15)`/`.min(63)`/`.min(255)`
NUL reserves not derived from the `#[repr(C)]` array lengths.

---

### D30 — smaller comments that lie — Low

| Site | Says | Actually |
|---|---|---|
| `src/transaction.rs:81` | "Panics in debug if `bytes.len() != block_size`" | returns `Err` (`:83-85`); never panics |
| `src/journal_writer.rs:14` | "performs five fenced steps" | the list at `:16-24` has four; `:142` says "four-fence" |
| `src/journal_writer.rs:136-139` | "the commit path enforces the real limit" | `commit` (`:165`) enforces the same loose `max_len - 1` |
| `src/inode.rs:127-128`, `:180-182` | extra section parsed when "size >= 160 and i_extra_isize >= 28" | code gates on `raw.len() >= 132` (`:183`) and per-field 4/8/12/16/20/24; neither 160 nor 28 appears |
| `src/block_cache.rs:140-141` | struct doc: "invalidates on writes" | `write_at:213-218` is write-**through**, and the module doc at `:23-27` says so |
| `src/fs.rs:1491-1493` | `buffer_patch_bgd_counters` "mirrors `patch_bgd_counters` byte for byte" | true — and the only difference is that the buffer copy **dropped all three field-offset comments** present at `:3611`, `:3622`, `:3633`. The surviving twin is the undocumented one |

---

### D31 — opaque names — Low

`alloc.rs:390-391` `fi`/`ud` (free_inodes / used_dirs) across a 20-line scoring block;
`verify.rs` threads `r` (the report) through six functions (`:115`, `:153`, `:232`,
`:278`, `:348`, `:450`); `htree.rs:130` `cl` for a `DxCountLimit`;
`block_cache.rs:162`/`:244` `s`.

The misleading pair worth fixing regardless: `inode.rs:290`/`:298` `join16`/`join32` —
the digit is the *lo* half's width, not the result's (`join16` returns a `u32`,
`join32` a `u64`).

---

### D32 — a loop-invariant guard buried six levels deep — Low

- **Files:** `src/verify.rs:422-445`, guard at `:439`, comment at `:420-421`

Nesting: `for slot` → `if p != 0 && p < total` → `if read_at.is_ok()` → `for i in 0..ppb`
→ `if inner != 0 && inner < total && slot >= 13`. `slot >= 13` does not vary within the
inner loop, so for `slot == 12` the block read at `:433` and the entire `ppb` iteration
run and discard everything — while the comment at `:420` claims single-indirect *is*
marked. Hoisting the condition removes both the wasted work and the contradiction.

---

# Test coverage

| Metric | Value |
|---|---|
| Unit tests in `src/` | 247 |
| Integration tests in `tests/` (107 files) | 536 |
| **Total** | **783** |
| `#[ignore]`d | 8 (2 are the ext3 mkfs oracle cases) |
| `cargo fmt --check` | clean **[verified]** |
| `cargo clippy --all-targets -D warnings` | clean per CI + pre-commit hook |

**Modules with no unit tests at all:**

| File | Lines | Covered by |
|---|---|---|
| `src/capi.rs` | 2,866 | 16 `tests/capi_*.rs` files — good end-to-end coverage |
| `src/fsck.rs` | 1,445 | `tests/fsck_repair.rs` (841 lines) |
| **`src/mkfs.rs`** | **1,026** | `mkfs_roundtrip`, `mkfs_e2fsck_oracle` (needs the VM), `ext2_basic` — **end-to-end only** |
| `src/file_io.rs` | 319 | integration suite |
| **`src/bin/mkfs_ext4.rs`** | **320** | `tests/mkfs_bin_smoke.rs` — **4 happy-path cases, no flag parsing** |
| `src/bgd.rs` / `error.rs` / `features.rs` / `inline_data.rs` / `lib.rs` | 646 | indirectly |

The two bolded rows are the reason Part B is sequenced the way it is. There is no test
anywhere for `parse_size`, `parse_uuid`, `set_bitmap_range`, `build_superblock`,
`write_bgd_group`, `build_jbd2_superblock` or `group_has_super` — all pure functions,
all trivially testable, all currently reachable only by formatting a whole image.

---

# What to fix first

The ordering is driven by one rule: **nothing gets refactored before the test that
would catch the refactor going wrong exists.**

### Round 0 — close the oracle gap (test-only, no production code touched)

1. **F2** — add a multi-group case to `tests/mkfs_e2fsck_oracle.rs` (256 MiB / 4 KiB
   gives 2 groups) and correct the stale "single-block-group only" header. This is the
   only item that changes what the crate *knows* rather than how it reads, and every
   other formatter item needs it as a safety net.
2. Add unit tests for the pure formatter helpers — `parse_size`, `parse_uuid`,
   `set_bitmap_range`, `group_has_super`. Cheap, and they are the functions F5/F6/F15
   want to change.

### Round 1 — the cheap, high-value corrections (small, mostly local)

3. **X1** — add `Checksummer::patch_dir_entry_tail` and route all nine sites through it.
   Twenty minutes, removes the highest-consequence duplication in the crate, and takes
   the formatter's copy (`mkfs.rs:433`) with it. Covered by existing tests on every site.
4. **F5** — move `-c` out of the argument-taking flag list and reconcile the help text
   with the parser. One line plus one doc line; add the regression test from Round 0's
   pattern.
5. **D5** — split `verify_inode`'s `||` so the short-buffer case fails closed like its
   three siblings.
6. **D4** — delete the three dead `fs.rs` functions and `CachingDevice`, or mark them
   `#[cfg(test)]` if they are wanted. ~300 lines, zero behaviour change, and it removes
   the trap of fixing the copy nothing calls. *(Deleting production code needs your
   explicit go-ahead — flagged, not assumed.)*
7. **F10, D23, D24, D25, D2, D3, D7** — the comment corrections. Free to make, and they
   are what a reader is currently being actively misled by. D2/D3/D7 in particular
   describe safety properties that a future caller will rely on.

### Round 2 — the structural work (test-gated, one at a time)

8. **D1** — decide whether the tag budget is actually bounded, then either enforce it at
   commit or correct the comment. This is the one item that might be a real bug rather
   than a readability problem; it deserves a `corroborated-debug` pass, not a refactor.
9. **F1 + F3 + F4** — the formatter split. Extract the shared geometry into one named
   struct (`Geometry { blocks_per_group, inodes_per_group, inode_table_blocks,
   first_data_block, reserved_inodes }`), have both formatters derive from it, and let
   `build_superblock` take that struct instead of 14 positional arguments. F3 stops being
   a latent trap the moment `first_data_block` is a field the multi-group path must fill in.
10. **D11 + X2** — collapse the seven inode-checksum stampers to one, using the named
    offsets. Do this before D10/D17, since it is the largest single block of what those
    functions duplicate.
11. **D10 + D17** — introduce the parameter struct for the three
    `extend_dir_and_add_entry*` functions and hoist the shared 65%.
12. **D8** — extract the callback-device builder; three copies to one, and it fixes the
    already-drifted error strings.
13. **D9** — resolve the htree reader/writer layout disagreement. `htree_mut` has no
    callers, so this is safe to do now and expensive to do after something starts
    calling it.
14. **F12** — split the ext3 formatter out of `format_filesystem_with_flavor`. Biggest
    single reduction in the flagship function's length, and the ext3 path is `#[ignore]`d
    at the oracle anyway.
15. **D29 / F7 / D13 / D26** — the nesting. Largely falls out of the items above; worth
    re-measuring rather than attacking directly.

### Deliberately not recommended

- `capi.rs`'s size (2,866 lines) and its ~470 lines of ABI boilerplate (D18). It is a
  flat list of entry points with a consistent shape — the one kind of long file that
  stays readable, because a reader looking for one function finds it by name and never
  needs the rest. The audit found no missing null check or `catch_unwind`. Only
  `fs_ext4_dir_next` (D18) needs attention, and that is a five-line fix, not a split.
- The 5+-parameter ABI entry points. The C header dictates them.

---

## What is good, and should survive any refactor

- **The comments explain WHY, and they explain the dangerous parts.** The
  must-not-read-from-disk invariant in `apply_pwrite`, the `first_data_block` bit-space
  hazard in `mkfs.rs:213-220`, the late-commit rationale at `fs.rs:4734`, the BGD
  bitmap-csum ordering note at `mkfs.rs:206-209` — every one is documented where it
  matters. The findings above are overwhelmingly about comments that were *true when
  written*; that is the failure mode of a codebase that comments well, not one that
  comments badly.
- **`tests/mkfs_e2fsck_oracle.rs` states its own reason for existing**, including the
  blind spot it was built to cover. F2 exists only because that header was so precise
  that its staleness is checkable.
- **The cross-validation posture is right.** Real `e2fsck` in a real Linux VM, plus the
  `lwext4` skeleton, means the crate is checked against the implementations it must
  agree with rather than against itself. The gap in F2 is one missing test case, not a
  missing strategy.
- **783 tests, `fmt` and `clippy -D warnings` clean, enforced by both CI and a
  pre-commit hook.**

---

*Nothing in this report has been applied. Tell me which items to take and in what
order, and I will run them through `dev-loop` one at a time — baseline captured first,
full suite green after each.*
