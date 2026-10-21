//! Reads at an offset, `read_at` and `read_exact_at`, on every platform.
//!
//! The signatures are those of `std::os::unix::fs::FileExt`, which std has for Unix only: the
//! store reads byte ranges of its files through them, with no `mmap` (storage-core design, §3).
//! On Windows the same read is std's `seek_read`, which moves the handle's cursor.
