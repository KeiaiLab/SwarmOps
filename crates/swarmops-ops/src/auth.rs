//! The user's logins: what `auth login`, `auth status` and `auth logout` do, on the one
//! user-level file that keeps a token per host ([`file()`]).

mod file;

pub use file::{AUTH_FILE_ENV, Provider, Source, file, login, logout, status};
