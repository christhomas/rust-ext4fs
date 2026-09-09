use fs_ext4::{block_io::BlockDevice, error::Result, runtime::Runtime, Filesystem};
use std::sync::{Arc, Mutex};
/// In-memory R/W block device backed by a single Vec<u8>.
struct MemDev {
    bytes: Mutex<Vec<u8>>,
    size: u64,
}

impl MemDev {
    fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(vec![0u8; size as usize]),
            size,
        })
    }
}

impl BlockDevice for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        assert!(end <= b.len(), "read past EOF");
        buf.copy_from_slice(&b[start..end]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        assert!(end <= b.len(), "write past EOF");
        b[start..end].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

struct Fixed;
impl Runtime for Fixed {
    fn now_unix_seconds(&self) -> u32 {
        1_700_000_123
    }
    fn next_inode_generation(&self) -> u32 {
        0x76543210
    }
}
#[test]
fn caller_runtime_controls_created_inode_metadata() {
    let size = 32 * 1024 * 1024;
    let dev = MemDev::new(size);
    fs_ext4::mkfs::format_filesystem(dev.as_ref(), None, Some([7; 16]), size, 4096).unwrap();
    let fs = Filesystem::mount_with_runtime(dev, Arc::new(Fixed)).unwrap();
    let ino = fs.apply_create("/runtime.txt", 0o600).unwrap();
    let (inode, raw) = fs.read_inode_verified(ino).unwrap();
    assert_eq!(inode.generation, 0x76543210);
    assert_eq!(
        u32::from_le_bytes(raw[0x0c..0x10].try_into().unwrap()),
        1_700_000_123
    );
}
