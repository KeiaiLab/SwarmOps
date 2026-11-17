//! The user's logins: what `auth login`, `auth status` and `auth logout` do, on the one
//! user-level file that keeps a token per host ([`file()`]); the credential a call to a git
//! remote goes with (`lookup`), which is where a login's token is used; and the answer
//! `auth git-credential` gives a git that asks for one (`helper`).

mod file;
mod helper;
mod lookup;

pub use file::{AUTH_FILE_ENV, Provider, Source, can_keep, file, login, logout, status};
pub use helper::{HelperAnswer, MAX_REQUEST_BYTES, git_credential};
pub(crate) use lookup::{credential, process_env};
