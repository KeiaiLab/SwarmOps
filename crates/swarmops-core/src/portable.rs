//! The checks for a name that every platform can hold (storage-core design, §2.1 P8).
//!
//! Windows cannot store some names as file names: a device name (`con`, `nul`, `com1`, with or
//! without an extension), a name that ends in a dot or a space, and a name with a character it
//! forbids. The rules are those of that platform, applied on every one, so that a command
//! behaves alike on all three and a store's refs and an export's trees can be files wherever
//! they go. Nothing here asks the host what it can hold, and this crate performs no I/O.

use thiserror::Error;

/// The names Windows reserves for devices in every directory: a file name whose stem is one of
/// them, in any ASCII case, names the device, not a file.
const DEVICE_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// The printable characters Windows refuses in a file name. It refuses the control characters,
/// U+0000 to U+001F, too.
const FORBIDDEN: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Why a component is not a name every platform can hold ([`check_component`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Unportable {
    /// A Windows device name ([`is_windows_device_name`]).
    #[error("a Windows device name")]
    DeviceName,
    /// It ends in a dot or a space, which Windows strips from a file name: `x.` would be `x`.
    #[error("it ends in a dot or a space")]
    TrailingDotOrSpace,
    /// A character Windows refuses in a file name: one of `\ / : * ? " < > |`, or a control
    /// character (U+0000 to U+001F).
    #[error("it holds the character {0:?}")]
    Character(char),
}

/// Whether Windows takes `component` for a device, so that no file can have that name: when its
/// stem, what comes before its first `.` or `:` with trailing spaces removed, is `con`, `prn`,
/// `aux`, `nul`, `com1` to `com9` or `lpt1` to `lpt9`, in any ASCII case (`CON`, `nul.txt`,
/// `Lpt9.tar.gz` and `con .txt` all are).
pub fn is_windows_device_name(component: &str) -> bool {
    let stem = component.split(['.', ':']).next().unwrap_or_default();
    let stem = stem.trim_end_matches(' ');
    DEVICE_NAMES
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
}

/// Checks that `component`, one name of a path, is one every platform can hold as a file or
/// directory name, in this order: it holds no character Windows refuses
/// ([`Unportable::Character`], the first one it holds), it is no device name
/// ([`Unportable::DeviceName`]), and it does not end in a dot or a space
/// ([`Unportable::TrailingDotOrSpace`]). An empty component breaks none of these rules: its
/// caller refuses it.
pub fn check_component(component: &str) -> Result<(), Unportable> {
    // The control characters are U+0000 to U+001F: the ones below a space.
    let refused = |c: &char| *c < ' ' || FORBIDDEN.contains(c);
    if let Some(c) = component.chars().find(refused) {
        return Err(Unportable::Character(c));
    }

    if is_windows_device_name(component) {
        return Err(Unportable::DeviceName);
    }

    if component.ends_with(['.', ' ']) {
        return Err(Unportable::TrailingDotOrSpace);
    }
    Ok(())
}
