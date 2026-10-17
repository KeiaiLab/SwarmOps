//! The core's integration tests, in one test binary (one link): each file of `tests/` is a
//! module here, and its tests are named after it (`codec::<test>`). Cargo builds no other test
//! target from `tests/` (`autotests = false` in `Cargo.toml`): a new test file is a `mod` line
//! here. `arbitrary.rs` needs the `arbitrary` feature, and is empty without it.

mod arbitrary;
mod codec;
mod id;
mod path;
