//! Abstract block-device I/O.
//!
//! The driver doesn't care if blocks come from a file, raw device, or a
//! callback into Swift — it just needs `read_at(offset, buf) -> Result<()>`.
//!
//! `write_at` is an optional trait method: it defaults to returning
//! `Error::Corrupt("read-only device")` so every existing read-only caller
//! keeps working. `FileDevice` and the callback-with-writer device override
//! it when the underlying resource allows writes.

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

/// Random-access block device. Reads required; writes optional.
pub trait BlockDevice: Send + Sync {
    /// Read exactly `buf.len()` bytes starting at `offset` (bytes from start of device).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Total device size in bytes (for bounds-checking).
    fn size_bytes(&self) -> u64;

    /// Write exactly `buf.len()` bytes at `offset`. Default: returns an error
    /// for read-only devices. Writable devices override this.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<()> {
        Err(Error::Corrupt(
            "block device is read-only (no write_at impl)",
        ))
    }

    /// Flush any pending writes to stable storage. Default: no-op for
    /// read-only devices; writable devices should implement fsync semantics.
    fn flush(&self) -> Result<()> {
        Ok(())
    }

    /// Reports whether `write_at` is likely to succeed. Used by the mount
    /// path to decide whether journal replay is possible.
    fn is_writable(&self) -> bool {
        false
    }

    /// Buffer-cache hook: stash `bytes` for `block` so a subsequent
    /// `read_at` returns those bytes instead of reading from physical
    /// storage. Used by `commit_block_buffer` to make journaled
    /// metadata visible to readers before the journal is checkpointed
    /// back to the data area on disk.
    ///
    /// Pinned entries inserted via `populate_cache` MUST NOT be evicted
    /// — they're the only place those bytes exist until `unpin_all`
    /// runs (typically after journal replay). Devices without a cache
    /// (raw `FileDevice`, etc.) implement this as a no-op and the
    /// caller's bytes simply have no in-memory shadow; that's safe
    /// because un-cached devices imply no separate journal log either.
    fn populate_cache(&self, _block: u64, _bytes: Vec<u8>) {}

    /// Buffer-cache hook: tell the device the journal has been
    /// checkpointed, so any blocks pinned via `populate_cache` are now
    /// consistent with disk and can be evicted under normal LRU
    /// pressure. No-op for un-cached devices.
    fn unpin_all(&self) {}
}

/// File-backed device — used for disk images and `/dev/diskN`.
pub struct FileDevice {
    file: Mutex<File>,
    size: u64,
    writable: bool,
}

impl FileDevice {
    /// Open read-only. Matches pre-existing behaviour.
    pub fn open(path: &str) -> Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            size,
            writable: false,
        })
    }

    /// Open read-write. Prefer this when the caller needs to journal-replay
    /// or apply Phase 4 mutations. Falls back to an error if the path is
    /// not writable.
    pub fn open_rw(path: &str) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            size,
            writable: true,
        })
    }

    /// Open read-write if possible; otherwise fall back to read-only. Useful
    /// for the mount path so read-only images on e.g. a locked volume still
    /// mount, just without replay.
    pub fn open_best_effort(path: &str) -> Result<Self> {
        match Self::open_rw(path) {
            Ok(d) => Ok(d),
            Err(_) => Self::open(path),
        }
    }
}

impl BlockDevice for FileDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)?;
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::Corrupt("FileDevice opened read-only"));
        }
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(buf)?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        if !self.writable {
            return Ok(());
        }
        let mut f = self.file.lock().unwrap();
        f.flush()?;
        f.sync_data()?;
        Ok(())
    }

    fn is_writable(&self) -> bool {
        self.writable
    }
}

/// Read callback: fill `buf` starting at byte `offset`.
pub type ReadCb = Box<dyn Fn(u64, &mut [u8]) -> std::io::Result<()> + Send + Sync>;
/// Write callback: write `buf` starting at byte `offset`.
pub type WriteCb = Box<dyn Fn(u64, &[u8]) -> std::io::Result<()> + Send + Sync>;
/// Flush callback.
pub type FlushCb = Box<dyn Fn() -> std::io::Result<()> + Send + Sync>;

/// Callback-backed device — used when the host process owns the fd
/// (e.g. FSBlockDeviceResource via the C bridge). Optional write callback;
/// set to `None` for read-only.
pub struct CallbackDevice {
    pub size: u64,
    pub read: ReadCb,
    pub write: Option<WriteCb>,
    pub flush: Option<FlushCb>,
}

impl BlockDevice for CallbackDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (self.read)(offset, buf)?;
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        match &self.write {
            Some(f) => {
                f(offset, buf)?;
                Ok(())
            }
            None => Err(Error::Corrupt("CallbackDevice has no write callback")),
        }
    }

    fn flush(&self) -> Result<()> {
        match &self.flush {
            Some(f) => {
                f()?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn is_writable(&self) -> bool {
        self.write.is_some()
    }
}

// ---------------------------------------------------------------------------
// CachingDevice — small LRU block cache decorator
// ---------------------------------------------------------------------------
