//! Fresh store roots and the file helpers the store's tests use on them; and, with the
//! `test-hooks` feature, the store-level error and crash sweeps' harness (`sweep`).

#[cfg(feature = "test-hooks")]
pub mod sweep;

mod frames;
mod root;

pub use frames::{lying_zstd, zstd_object, zstd_v07_frame};
pub use root::{append_bytes, file_len, layout_of, new_root, real_fs, set_len, temp_dir};
