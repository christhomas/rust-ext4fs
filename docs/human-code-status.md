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

### D1, D3 — the journal tag budget rests on a premise the same function contradicts; "atomic" rename is not atomic on the overwrite path — High

**D3 is the one to look at soonest.** It is a correctness claim in a doc, about
the property callers most rely on, and unlike D2 it is not merely stale — the
overwrite path genuinely is not atomic.

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
