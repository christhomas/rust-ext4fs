# Checked journal recovery and release

`Filesystem::mount_recovering` opens an exclusively owned writable device,
recovers committed journal records and supported orphan operations, and returns
fresh superblock/group metadata. Call `finish(self)` before releasing it to
another owner. Dropping a handle does not claim a clean release.

The ext4 `RECOVER` / `needs_recovery` flag is expected while a journaled filesystem
is mounted. Its presence alone is neither filesystem damage nor a requirement to
format. A writable mount may change storage before the first requested file
operation. Applications requiring a backup must save and independently reopen it
**before** calling this API. A read-only mount does not replay anything.

The checked lifecycle currently accepts an internal JBD2 v2 journal with matching
block size, first log block 1, one user, no compatibility/read-only flags and only
REVOKE/64BIT incompatibility flags. Outstanding journal/filesystem errors are
reported. Checksummed, async-commit, fast-commit and unknown journal formats remain
unsupported in this API. The older `mount` interface remains available for
compatibility; its checksummed transaction encoding/recovery is not qualified by
these tests. Ext4 metadata checksums are distinct from JBD2 transaction checksums.

Recovery collects descriptor writes and revoke records only after their commit
block. Incomplete final transactions, including their revokes, are discarded.
Transaction comparisons wrap as JBD2 sequence numbers do. Replay writes are
flushed before the journal's clean cursor is written and flushed. The filesystem
RECOVER bit remains set throughout writable ownership; `finish` clears it only
after flush, a clean journal and an empty orphan chain.

Any write/flush failure is an error. Failed journal writers cannot accept another
transaction. Release the failed handle and reopen under fresh ownership. A failure
at the final clean-marker flush can leave the marker clean or dirty; either is
recoverable because data was flushed first. The driver makes no recovery writes
after an I/O error in an attempt to guess which state reached the device.

Qualification uses Linux `mkfs.ext4`/`debugfs` and independently encoded plain
JBD2 records. `e2fsck` agrees on a committed inode/superblock update followed by
an uncommitted overwrite or revoke. The first mount observes refreshed metadata,
finished images pass `e2fsck -fn` without skipping recovery, and repeat mount/
finish is byte-stable. Every one of nine write/flush boundaries is interrupted
under both immediate-durability and volatile-until-flush models (18 cases), then
independently recovered by `e2fsck`. These tests do not model torn sectors, lying
flush acknowledgements, concurrent writers or real USB transports.

References: [Linux JBD2 recovery](https://github.com/torvalds/linux/blob/master/fs/jbd2/recovery.c)
and [ext4 journal format](https://www.kernel.org/doc/html/latest/filesystems/ext4/journal.html).
