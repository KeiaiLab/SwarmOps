//! The user's logins: what `auth login`, `auth status` and `auth logout` do, on the one
//! user-level file that keeps a token per host ([`file()`]), and the credential a call to a
//! git remote goes with (`lookup`), which is where a login's token is used.

mod file;
mod lookup;

pub use file::{AUTH_FILE_ENV, Provider, Source, file, login, logout, status};
pub(crate) use lookup::{credential, process_env};
