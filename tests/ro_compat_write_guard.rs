//! A feature this driver does not maintain permits reading and refuses
//! writing.
//!
//! `RO_COMPAT` states one rule with two halves: an implementation that
//! does not know the bit may READ the filesystem and must not WRITE it.
//! This driver enforced the first half and not the second — its own
//! `check_mountable` comment says "mounted read-only", which stopped
//! being true a long time ago — so a volume carrying such a bit was
//! mounted writable, and a create updated what the driver knows about
//! and silently left the rest.
//!
//! The case to picture is `QUOTA`, which is not hypothetical: the
//! string "quota" appears in `src/features.rs` and nowhere else in the
//! crate, so a create on a quota-enabled volume charged nobody for the
//! file and left counters that no longer describe the filesystem.

use fs_ext4::checksum::linux_crc32c;
use fs_ext4::features::RoCompat;
use fs_ext4::{Error, Filesystem};
use std::sync::Arc;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-disks/ext4-basic.img");

/// The superblock starts 1024 bytes in; `s_feature_ro_compat` is at
/// 0x64 within it, and `s_checksum` at 0x3FC.
const SB_AT: usize = 1024;
const RO_COMPAT_AT: usize = SB_AT + 0x64;
const CSUM_AT: usize = SB_AT + 0x3FC;

/// A writable in-memory device, so a fixture can be edited without
/// touching the file on disk.
struct MemDev {
    bytes: std::sync::Mutex<Vec<u8>>,
}

impl MemDev {
    fn arc(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            bytes: std::sync::Mutex::new(bytes),
        })
    }
}

impl fs_ext4::block_io::BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > b.len() {
            return Err(Error::OutOfBounds);
        }
        buf.copy_from_slice(&b[start..end]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        if start + buf.len() > b.len() {
            return Err(Error::OutOfBounds);
        }
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A copy of the fixture with `bits` added to `s_feature_ro_compat` and
/// the superblock checksum restored, so the volume is well-formed and
/// differs from the original only in the feature mask.
fn fixture_with_ro_compat(bits: u32) -> Arc<MemDev> {
    edited_fixture(bits, 0)
}

/// The same, with `clear` removed from the mask as well as `set` added.
fn edited_fixture(set: u32, clear: u32) -> Arc<MemDev> {
    let mut bytes = std::fs::read(IMAGE).expect("read the fixture");
    let existing = u32::from_le_bytes(bytes[RO_COMPAT_AT..RO_COMPAT_AT + 4].try_into().unwrap());
    let bits = set;
    let updated = (existing | bits) & !clear;
    bytes[RO_COMPAT_AT..RO_COMPAT_AT + 4].copy_from_slice(&updated.to_le_bytes());

    // METADATA_CSUM is set on this fixture, so the superblock carries a
    // checksum over everything before it. Leave it stale and the mount
    // fails for the wrong reason entirely.
    let csum = linux_crc32c(!0, &bytes[SB_AT..CSUM_AT]);
    bytes[CSUM_AT..CSUM_AT + 4].copy_from_slice(&csum.to_le_bytes());
    MemDev::arc(bytes)
}

/// The bits worth testing, and why each one.
fn cases() -> Vec<(&'static str, u32)> {
    vec![
        // Known, tolerated for reading, and NOT maintained: nothing in
        // this crate touches the quota inodes.
        ("quota", RoCompat::QUOTA.bits()),
        // Known and not maintained either — the orphan file is read and
        // not kept up to date.
        ("orphan_present", RoCompat::ORPHAN_PRESENT.bits()),
        // Nothing at all yet, which is what a future feature looks like
        // from here and the case the rule exists for.
        ("an unassigned bit", 1 << 28),
    ]
}

/// Every one of them still mounts and reads.
///
/// The mount path must not get stricter than the format. Refusing to
/// read a volume because of a `RO_COMPAT` bit would lock a user out of
/// data that is perfectly readable, which is the opposite of what the
/// bit says.
#[test]
fn an_unmaintained_ro_compat_bit_still_reads() {
    for (name, bits) in cases() {
        let dev = fixture_with_ro_compat(bits);
        let fs = Filesystem::mount(dev)
            .unwrap_or_else(|e| panic!("{name}: the volume must still be readable, got {e:?}"));
        // Reading the root inode is enough to say the volume is
        // readable: it goes through the superblock, the group
        // descriptors and the inode table, which is every structure the
        // mount just validated.
        let (inode, _raw) = fs
            .read_inode_verified(2)
            .unwrap_or_else(|e| panic!("{name}: reading the root inode failed: {e:?}"));
        assert!(
            inode.size > 0,
            "{name}: the root inode came back describing nothing"
        );
    }
}

/// And every one of them refuses a write, naming the bit.
#[test]
fn an_unmaintained_ro_compat_bit_refuses_a_write() {
    for (name, bits) in cases() {
        let dev = fixture_with_ro_compat(bits);
        let fs = Filesystem::mount(dev).expect("mount");
        match fs.apply_create("/newfile.txt", 0o644) {
            Err(Error::UnsupportedRoCompat(reported)) => {
                assert_eq!(
                    reported & bits,
                    bits,
                    "{name}: the refusal should name the bit that caused it"
                );
            }
            Err(other) => panic!("{name}: wrong refusal {other:?}"),
            Ok(_) => panic!("{name}: a create must be refused on this volume"),
        }
    }
}

/// An ordinary volume is not caught by the guard.
///
/// This is how the check is most likely to go wrong: a mask that is too
/// tight refuses volumes `mke2fs` makes with its defaults, and the
/// failure would look like the driver breaking rather than a feature
/// being refused.
#[test]
fn a_default_volume_still_writes() {
    let bytes = std::fs::read(IMAGE).expect("read the fixture");
    let fs = Filesystem::mount(MemDev::arc(bytes)).expect("mount");
    fs.apply_create("/guard_smoke.txt", 0o644)
        .expect("an ordinary volume must still accept a create");
}

/// A volume with `GDT_CSUM` and not `METADATA_CSUM` refuses a write.
///
/// This one was in the maintained set when the guard first landed, and
/// it should not have been. The mistake is worth recording because the
/// bit *looks* maintained from the feature table: group-descriptor
/// checksums are computed and verified elsewhere in the crate, so it
/// reads as covered.
///
/// It is not. `Checksummer::from_superblock` sets `enabled` from
/// `METADATA_CSUM` alone, and the only writer of a descriptor checksum,
/// `buffer_patch_bgd_counters`, sits behind `if self.csum.enabled`. So
/// on a volume with `GDT_CSUM` and not `METADATA_CSUM` this driver
/// edited group descriptors and left every checksum stale, which is the
/// same shape of harm as the `QUOTA` case the guard was built for.
///
/// The two are not even the same algorithm: `METADATA_CSUM` descriptors
/// are crc32c and `GDT_CSUM` descriptors are crc16, which this crate
/// does not implement. So this is a refusal until that exists, not an
/// oversight to be patched by flipping a flag.
///
/// `mke2fs` produced exactly this combination by default before 1.43,
/// so the volume being refused here is an ordinary older disk rather
/// than a contrived one.
#[test]
fn a_gdt_csum_volume_without_metadata_csum_refuses_a_write() {
    let dev = edited_fixture(RoCompat::GDT_CSUM.bits(), RoCompat::METADATA_CSUM.bits());
    let fs = Filesystem::mount(dev).expect("the volume must still be readable");

    // Readable: the group descriptors parse and the root resolves.
    let (inode, _raw) = fs.read_inode_verified(2).expect("read the root inode");
    assert!(
        inode.size > 0,
        "the root inode came back describing nothing"
    );

    match fs.apply_create("/gdt_csum.txt", 0o644) {
        Err(Error::UnsupportedRoCompat(bits)) => {
            assert_eq!(
                bits & RoCompat::GDT_CSUM.bits(),
                RoCompat::GDT_CSUM.bits(),
                "the refusal should name GDT_CSUM"
            );
        }
        Err(other) => panic!("wrong refusal: {other:?}"),
        Ok(_) => panic!(
            "a create must be refused: this driver would leave every \
             group-descriptor checksum it touched stale"
        ),
    }
}
