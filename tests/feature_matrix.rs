//! Every feature bit the crate has an opinion about, checked against a
//! filesystem that actually carries it.
//!
//! G5 from `docs/format-conformance-gaps.md`. Every gap that document
//! records was found by *reading* the supported-feature masks and their
//! comments — not one was caught by a test. This is the test that would
//! have caught them.
//!
//! The contract per feature is deliberately weak: **read it correctly,
//! or refuse to mount.** Both are acceptable; what is not acceptable is
//! mounting and then returning data derived from arithmetic the feature
//! invalidates, which is the failure mode every gap in that document
//! shares.
//!
//! # Why this builds its own image
//!
//! `test-disks/*.img` is gitignored, and the existing builder needs an
//! Alpine VM because most fixtures require a real mount to populate.
//! The images here need only `mke2fs` — nothing is written into them,
//! the feature bits in the superblock are the whole point — so the test
//! builds them itself and runs anywhere e2fsprogs is installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::Filesystem;

/// ext4's root directory is always inode 2.
const ROOT_INO: u32 = 2;

/// Locate `mke2fs`. Homebrew keeps e2fsprogs keg-only on macOS, so it
/// is present but not on PATH.
fn mke2fs() -> Option<String> {
    if Command::new("mke2fs").arg("-V").output().is_ok() {
        return Some("mke2fs".into());
    }
    let brew = Command::new("brew")
        .args(["--prefix", "e2fsprogs"])
        .output()
        .ok()?;
    let prefix = String::from_utf8(brew.stdout).ok()?;
    let path = format!("{}/sbin/mke2fs", prefix.trim());
    Path::new(&path).exists().then_some(path)
}

/// Build a 16 MiB image with the given `mke2fs` options.
fn build(name: &str, opts: &[&str]) -> Option<PathBuf> {
    let mke2fs = mke2fs()?;
    let path = std::env::temp_dir().join(format!("fs-ext4-fm-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, vec![0u8; 16 * 1024 * 1024]).expect("allocate image");
    let out = Command::new(&mke2fs)
        .args(["-q", "-F"])
        .args(opts)
        .arg(&path)
        .output()
        .expect("run mke2fs");
    assert!(
        out.status.success(),
        "mke2fs {opts:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(path)
}

fn mount(path: &Path) -> fs_ext4::error::Result<Filesystem> {
    let dev = FileDevice::open(path.to_str().expect("utf-8 path")).expect("open image");
    let dyn_dev: Arc<dyn BlockDevice> = Arc::new(dev);
    Filesystem::mount(dyn_dev)
}

/// **The regression G1 fixed.**
///
/// bigalloc moves the allocation unit from the block to the cluster, so
/// every block-group offset this crate computes is wrong on such a
/// filesystem. Until the cluster arithmetic exists, refusing is the
/// only correct behaviour — mounting means returning data from wrong
/// addresses with nothing reporting it.
///
/// Built with `-C 16384` against a 4 KiB block, so the cluster is four
/// blocks and the divergence appears in the first group rather than
/// somewhere deep in the filesystem.
///
/// # What happens without the refusal, measured
///
/// Disabling the bigalloc check and running this test produces:
///
/// ```text
/// BadChecksum { what: "block group descriptor" }
/// ```
///
/// That is the misreading, caught in the act: the crate located the
/// group descriptors with block arithmetic, read cluster-based data,
/// and the checksum did not match.
///
/// **That it was caught at all is luck, not design.** `metadata_csum`
/// is optional; on a bigalloc filesystem without it the same wrong
/// read produces no error and the caller gets whatever those bytes
/// happened to be. The checksum is a backstop that happens to fire
/// here, not a substitute for refusing a format the reader cannot
/// address.
///
/// It also mislabels the problem — a user seeing "bad checksum" goes
/// looking for corruption in a filesystem that is perfectly intact.
#[test]
fn bigalloc_is_refused_rather_than_misread() {
    let Some(img) = build("bigalloc", &["-t", "ext4", "-O", "bigalloc", "-C", "16384"]) else {
        eprintln!("skip: mke2fs not available (apt/brew install e2fsprogs)");
        return;
    };
    let result = mount(&img);
    let _ = std::fs::remove_file(&img);

    // Assert the SPECIFIC refusal, not merely that something failed.
    // `is_err()` alone passed even with the bigalloc check disabled,
    // because the image was being rejected for an unrelated reason —
    // a test that cannot tell those apart is not testing the fix.
    match result {
        Err(fs_ext4::error::Error::UnsupportedRoCompat(bits)) => {
            assert_ne!(
                bits & fs_ext4::features::RoCompat::BIGALLOC.bits(),
                0,
                "refused, but not for bigalloc: {bits:#x}"
            );
        }
        Err(other) => panic!(
            "a bigalloc filesystem was refused, but for the wrong reason: {other:?}. \
             The refusal must name bigalloc, or a later change that removes the \
             bigalloc check will still look green."
        ),
        Ok(_) => panic!(
            "a bigalloc filesystem MOUNTED. This crate does not implement cluster \
             arithmetic, so every block-group offset it computes on such a \
             filesystem is wrong — mounting means serving data from wrong \
             addresses."
        ),
    }
}

/// The guard against fixing G1 too broadly.
///
/// It would be easy to refuse bigalloc by tightening the RO_COMPAT
/// check into "anything unrecognised", which would also refuse ordinary
/// filesystems carrying newer bits. This mounts a plain ext4 and
/// requires it to still work.
#[test]
fn an_ordinary_ext4_still_mounts_and_reads() {
    let Some(img) = build("plain", &["-t", "ext4"]) else {
        eprintln!("skip: mke2fs not available");
        return;
    };
    let fs = mount(&img).expect("a plain ext4 filesystem must mount");
    fs.read_inode_verified(ROOT_INO)
        .expect("the root inode must be readable after mounting");
    let _ = std::fs::remove_file(&img);
}

/// An RO_COMPAT bit the crate has never heard of must still mount —
/// that is the compatibility model, and the reason G1's refusal had to
/// be a named list rather than a mask complement.
///
/// `project` is a real RO_COMPAT feature the reader does nothing with.
#[test]
fn a_tolerated_ro_compat_feature_still_mounts() {
    let Some(img) = build("project", &["-t", "ext4", "-O", "project,quota"]) else {
        eprintln!("skip: mke2fs not available");
        return;
    };
    let fs = mount(&img).expect("project/quota are ignorable on a read-only mount");
    fs.read_inode_verified(ROOT_INO).expect("root readable");
    let _ = std::fs::remove_file(&img);
}
