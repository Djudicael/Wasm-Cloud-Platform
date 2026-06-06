use std::sync::atomic::Ordering;

use super::{PolicyDenied, PolicyEnforcer};

impl PolicyEnforcer {
    /// Check if opening a file descriptor is allowed and atomically reserve a slot.
    pub fn check_fd_open(&self) -> Result<(), PolicyDenied> {
        let limit = self.policy.filesystem.max_open_fds;
        loop {
            let current = self.counters.open_fds.load(Ordering::Acquire);
            if current >= limit {
                self.counters
                    .fd_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(PolicyDenied::FdLimitExceeded { current, limit });
            }
            if self
                .counters
                .open_fds
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                Self::update_peak(&self.counters.open_fds_peak, current + 1);
                break;
            }
        }
        Ok(())
    }

    /// Record that an FD was opened.
    pub fn record_fd_open(&self) {
        self.counters.fd_open_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an FD was closed.
    pub fn record_fd_close(&self) {
        let prev = self.counters.open_fds.fetch_sub(1, Ordering::AcqRel);
        if prev == 0 {
            // Underflow - correct back to 0.
            self.counters.open_fds.store(0, Ordering::Release);
        }
    }

    /// Check if a filesystem write is allowed.
    #[deprecated(
        since = "0.2.0",
        note = "Use check_and_record_fs_write instead to avoid TOCTOU races"
    )]
    pub fn check_fs_write(&self, additional_bytes: u64) -> Result<(), PolicyDenied> {
        if self.policy.filesystem.max_fs_write_bytes == 0 {
            return Ok(());
        }

        let current = self.counters.fs_write_bytes.load(Ordering::Relaxed);
        if current + additional_bytes > self.policy.filesystem.max_fs_write_bytes {
            self.counters
                .fs_write_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::FsWriteLimitExceeded {
                current,
                requested: additional_bytes,
                limit: self.policy.filesystem.max_fs_write_bytes,
            });
        }
        Ok(())
    }

    /// Record filesystem write bytes.
    #[deprecated(
        since = "0.2.0",
        note = "Use check_and_record_fs_write instead to avoid TOCTOU races"
    )]
    pub fn record_fs_write(&self, bytes: u64) {
        self.counters
            .fs_write_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Atomically check if a filesystem write is allowed and record the bytes.
    pub fn check_and_record_fs_write(&self, bytes: u64) -> Result<(), PolicyDenied> {
        let limit = self.policy.filesystem.max_fs_write_bytes;
        if limit == 0 {
            self.counters
                .fs_write_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            return Ok(());
        }

        loop {
            let current = self.counters.fs_write_bytes.load(Ordering::Acquire);
            let new_val = current + bytes;
            if new_val > limit {
                self.counters
                    .fs_write_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(PolicyDenied::FsWriteLimitExceeded {
                    current,
                    requested: bytes,
                    limit,
                });
            }
            if self
                .counters
                .fs_write_bytes
                .compare_exchange(current, new_val, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        Ok(())
    }

    /// Check if creating a file is allowed.
    pub fn check_file_create(&self) -> Result<(), PolicyDenied> {
        if !self.policy.filesystem.allow_file_create {
            return Err(PolicyDenied::FileCreateDenied);
        }
        self.counters
            .file_creates_total
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Check if deleting a file is allowed.
    pub fn check_file_delete(&self) -> Result<(), PolicyDenied> {
        if !self.policy.filesystem.allow_file_delete {
            return Err(PolicyDenied::FileDeleteDenied);
        }
        self.counters
            .file_deletes_total
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
