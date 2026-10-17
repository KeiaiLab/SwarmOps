//! The API's integration tests, in one test binary (one link): each file of `tests/` is a
//! module here, and its tests are named after it (`schema::<test>`). Cargo builds no other test
//! target from `tests/` (`autotests = false` in `Cargo.toml`): a new test file is a `mod` line
//! here.

mod identity;
mod schema;
