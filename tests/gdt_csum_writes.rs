//! Independent Linux oracle for legacy CRC16 group descriptors.
#![cfg(target_os = "linux")]
use fs_ext4::{block_io::FileDevice, Filesystem};
use std::{
    process::Command,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
static ID: AtomicU32 = AtomicU32::new(1);
fn oracle(block_size: u32, descriptor64: bool) {
    let p = std::env::temp_dir().join(format!(
        "ext4-gdt-{}-{}.img",
        std::process::id(),
        ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::File::create(&p)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let features = if descriptor64 {
        "^metadata_csum,uninit_bg,64bit"
    } else {
        "^metadata_csum,uninit_bg,^64bit"
    };
    let out = Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            &block_size.to_string(),
            "-O",
            features,
            "-E",
            "lazy_itable_init=0,lazy_journal_init=0",
        ])
        .arg(&p)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    {
        let fs =
            Filesystem::mount(Arc::new(FileDevice::open_rw(p.to_str().unwrap()).unwrap())).unwrap();
        assert_ne!(fs.sb.feature_ro_compat & 0x10, 0);
        assert!(!fs.csum.enabled);
        fs.apply_create("/unrelated.txt", 0o600).unwrap();
        fs.apply_pwrite("/unrelated.txt", 0, b"preserve this")
            .unwrap();
        fs.apply_mkdir("/temporary", 0o755).unwrap();
        fs.apply_create("/temporary/stage.fat", 0o600).unwrap();
        let payload = vec![0x5a; 32 * 1024 * 1024];
        fs.apply_pwrite("/temporary/stage.fat", 0, &payload)
            .unwrap();
        fs.apply_rename("/temporary/stage.fat", "/efisp.fat", false)
            .unwrap();
        fs.apply_create("/temporary/remove", 0o600).unwrap();
        fs.apply_unlink("/temporary/remove").unwrap();
        fs.apply_rmdir("/temporary").unwrap();
        fs.dev.flush().unwrap();
    }
    let out = Command::new("e2fsck")
        .args(["-fn"])
        .arg(&p)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}: {}\n{}",
        p.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new("debugfs")
        .args(["-R", "cat /unrelated.txt"])
        .arg(&p)
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"preserve this");
    std::fs::remove_file(p).unwrap();
}
#[test]
fn gdt_crc16_32byte_4k() {
    oracle(4096, false);
}
#[test]
fn gdt_crc16_64byte_4k() {
    oracle(4096, true);
}
#[test]
fn gdt_crc16_32byte_1k() {
    oracle(1024, false);
}
#[test]
fn gdt_crc16_64byte_1k() {
    oracle(1024, true);
}
