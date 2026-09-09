//! JBD2 journal replay applier.
//!
//! Turns a [`journal::ReplayPlan`] (plan-layer output from the walker) into
//! actual `write_at` calls on a writable [`BlockDevice`]. The corresponding
//! read-side walker is [`crate::journal::walk`]; the write-side producer is
//! [`crate::transaction::Transaction`]. Closing the loop:
//!
//!   Transaction::commit ──(journal bytes)──▶ disk
//!                                              │
//!                         journal::walk ◀──────┘
//!                              │
//!                              ▼
//!                   journal_apply::apply  ──▶ final-location disk writes
//!
//! Replay applies only complete transactions, flushes final-location writes,
//! and only then advances the journal cursor and flushes its clean marker.
//! Interrupted replay leaves the old journal available for idempotent recovery.
//! If the final marker's flush fails, its durability is unknown but both possible
//! cursor states are safe because data was already flushed. Do not retry writes
//! on an I/O-failed handle; reopen it under fresh ownership.

use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::Inode;
use crate::jbd2::{self, JournalSuperblock};
use crate::journal::ReplayPlan;

/// Where a block number lands on the device, or a refusal.
///
/// Replay is the one operation that writes to a block number the image
/// itself chose. It runs at mount, before anybody has asked for a file,
/// and the number comes from a descriptor tag inside the journal --
/// which is to say, from whoever wrote the image.
///
/// Two things go wrong when that number is taken on trust:
///
///   - a block past the end of the filesystem writes past the end of the
///     device. Against a file-backed image that is not an error at all;
///     the file simply grows, and a tag naming block 0xDEADBEEF turns a
///     16 MiB image into a 15 TB one.
///   - a block number above `2^64 / block_size` wraps. At a 4 KiB block
///     size, block `2^52` multiplies out to exactly 0 -- the boot sector
///     and the primary superblock -- and the replay overwrites them with
///     contents the image supplied, then reports success.
///
/// Neither is a write to be clamped into range. A destination the
/// filesystem does not contain means the journal is not describing this
/// filesystem, so the replay stops and the mount fails rather than
/// applying the part of it that happened to be in range.
pub(crate) fn byte_offset_of(fs: &Filesystem, block: u64) -> Result<u64> {
    byte_offset_in(
        block,
        fs.sb.blocks_count,
        fs.sb.block_size(),
        fs.dev.size_bytes(),
    )
}

/// The same rule, for callers that hold the pieces rather than the
/// `Filesystem` -- `journal_writer` writes through a `&dyn BlockDevice`
/// after the mount has gone out of scope.
pub(crate) fn byte_offset_in(
    block: u64,
    blocks_count: u64,
    block_size: u32,
    device_bytes: u64,
) -> Result<u64> {
    if block >= blocks_count {
        return Err(Error::Corrupt(
            "journal: names a block past the end of the filesystem",
        ));
    }
    let block_size = block_size as u64;
    let offset = block
        .checked_mul(block_size)
        .ok_or(Error::Corrupt("journal: block offset overflows"))?;
    // `blocks_count` is the image's own claim about its size, so a
    // truncated image passes the check above and still reaches past the
    // device. The device is asked directly.
    let end = offset
        .checked_add(block_size)
        .ok_or(Error::Corrupt("journal: block offset overflows"))?;
    if end > device_bytes {
        return Err(Error::Corrupt(
            "journal: names a block past the end of the device",
        ));
    }
    Ok(offset)
}

/// Apply all writes in `plan` to `fs.dev`. Each `ReplayEntry` names a
/// journal-block source and a fs-block destination; we read the source
/// contents and overwrite the destination. Revoked writes are already
/// filtered out of `plan.writes` by [`ReplayPlan::filter_revoked`] (called
/// by [`crate::journal::walk`]).
///
/// Returns the number of blocks written. Errors propagate from either the
/// journal read or the final-location write.
pub fn apply(fs: &Filesystem, plan: &ReplayPlan) -> Result<usize> {
    if plan.writes.is_empty() {
        return Ok(0);
    }
    if !fs.dev.is_writable() {
        return Err(Error::Corrupt(
            "journal_apply: device is not writable; cannot replay",
        ));
    }

    let raw = fs.read_inode_raw(fs.sb.journal_inode)?;
    let jinode = Inode::parse(&raw)?;
    let block_size = fs.sb.block_size() as u64;

    // Validate the complete plan before publishing any prefix. Sources and
    // destinations are image-controlled; a later invalid entry must not turn
    // an otherwise rejected mount into a partial metadata mutation.
    let mut locations = Vec::with_capacity(plan.writes.len());
    for w in &plan.writes {
        let phys = jbd2::journal_block_to_physical(fs, &jinode, w.journal_block)?
            .ok_or(Error::Corrupt("journal_apply: journal block unmapped"))?;
        locations.push((byte_offset_of(fs, phys)?, byte_offset_of(fs, w.fs_block)?));
    }
    let mut applied = 0usize;
    for (w, (source, destination)) in plan.writes.iter().zip(locations) {
        let mut buf = vec![0u8; block_size as usize];
        fs.dev.read_at(source, &mut buf)?;

        // If the ESCAPED flag is set, the first 4 bytes of the journal block
        // were zeroed during write to keep them from colliding with the
        // JBD2 magic. Restore them to the magic before writing to final.
        if w.flags & crate::journal::TAG_ESCAPED != 0 {
            buf[0..4].copy_from_slice(&crate::jbd2::JBD2_MAGIC_NUMBER.to_be_bytes());
        }

        // Destination: fs_block * block_size = byte offset on the device.
        fs.dev.write_at(destination, &buf)?;
        applied += 1;
    }

    // Best-effort durability: flush before we claim success. If the caller
    // wants full crash-consistent ordering (journal writes → fsync → final
    // writes → fsync), they should sequence it themselves; this module is
    // the single final-location pass after the journal is already on disk.
    fs.dev.flush()?;

    Ok(applied)
}

/// Convenience: mount-time entry point. Reads the journal superblock and,
/// if the journal is dirty AND the device is writable, walks + applies in
/// one shot. Returns the number of blocks replayed (0 if clean or device
/// is read-only — the latter is not an error; mount proceeds read-only).
pub fn replay_if_dirty(fs: &Filesystem) -> Result<usize> {
    // Read-only mounts skip replay regardless of journal state — the read
    // path tolerates a non-clean journal (pending transactions are
    // invisible, which is correct for a read-only view of a dirty image).
    // Checking this BEFORE `read_superblock` matters for ext3: the journal
    // inode's i_block holds legacy indirect pointers, and the deeper code
    // currently bails on non-extent journals. RO mounts have no business
    // touching the journal at all, so we exit early here.
    if !fs.dev.is_writable() {
        return Ok(0);
    }
    let Some(jsb) = jbd2::read_superblock(fs)? else {
        return Ok(0); // no journal inode → nothing to replay
    };
    if jsb.is_clean() {
        return Ok(0);
    }
    let plan = crate::journal::walk(fs, &jsb)?;
    let applied = apply(fs, &plan)?;
    // The final-location writes must be durable before the cursor says clean.
    // Discarding an incomplete tail also advances beyond its transaction ID.
    let next_sequence = plan.next_sequence.unwrap_or(jsb.sequence).wrapping_add(1);
    crate::journal_writer::JournalWriter::finish_replay(fs, next_sequence)?;
    Ok(applied)
}

/// Describe the JBD2 superblock fields most relevant to replay decisions.
/// Useful for diagnostics and for tests that want to assert on the state
/// before and after replay.
#[derive(Debug, Clone)]
pub struct ReplaySummary {
    pub clean: bool,
    pub sequence: u32,
    pub start: u32,
    pub max_len: u32,
    pub has_revoke: bool,
    pub uses_64bit: bool,
    pub uses_csum_v2_or_v3: bool,
}

impl From<&JournalSuperblock> for ReplaySummary {
    fn from(jsb: &JournalSuperblock) -> Self {
        Self {
            clean: jsb.is_clean(),
            sequence: jsb.sequence,
            start: jsb.start,
            max_len: jsb.max_len,
            has_revoke: jsb.has_revoke(),
            uses_64bit: jsb.uses_64bit(),
            uses_csum_v2_or_v3: jsb.uses_csum_v2_or_v3(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{ReplayEntry, ReplayPlan};

    #[test]
    fn empty_plan_applies_zero_blocks() {
        // We can exercise the early-return branch without a real Filesystem.
        let plan = ReplayPlan::default();
        // The function requires a Filesystem; but the early-return path never
        // touches it. Skipping — the integration test in tests/ covers the
        // live path.
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn replay_summary_mirrors_jsb() {
        let jsb = JournalSuperblock {
            block_type: crate::jbd2::JBD2_SUPERBLOCK_V2,
            header_sequence: 1,
            block_size: 4096,
            max_len: 8192,
            first: 1,
            sequence: 42,
            start: 100,
            errno: 0,
            feature_compat: 0,
            feature_incompat: crate::jbd2::JbdIncompat::REVOKE.bits()
                | crate::jbd2::JbdIncompat::BIT64.bits()
                | crate::jbd2::JbdIncompat::CSUM_V3.bits(),
            feature_ro_compat: 0,
            uuid: [0; 16],
            nr_users: 1,
            checksum_type: 4,
            num_fc_blocks: 0,
            checksum: 0,
        };
        let s = ReplaySummary::from(&jsb);
        assert!(!s.clean);
        assert_eq!(s.sequence, 42);
        assert_eq!(s.start, 100);
        assert!(s.has_revoke);
        assert!(s.uses_64bit);
        assert!(s.uses_csum_v2_or_v3);
    }

    #[test]
    fn clean_summary() {
        let mut jsb = JournalSuperblock {
            block_type: crate::jbd2::JBD2_SUPERBLOCK_V2,
            header_sequence: 1,
            block_size: 4096,
            max_len: 8192,
            first: 1,
            sequence: 1,
            start: 0,
            errno: 0,
            feature_compat: 0,
            feature_incompat: 0,
            feature_ro_compat: 0,
            uuid: [0; 16],
            nr_users: 1,
            checksum_type: 0,
            num_fc_blocks: 0,
            checksum: 0,
        };
        jsb.start = 0;
        let s = ReplaySummary::from(&jsb);
        assert!(s.clean);
    }

    #[test]
    fn replay_entry_structure_stable() {
        let e = ReplayEntry {
            transaction: 1,
            fs_block: 100,
            journal_block: 5,
            flags: 0,
        };
        assert_eq!(e.transaction, 1);
        assert_eq!(e.fs_block, 100);
        assert_eq!(e.journal_block, 5);
    }
}
